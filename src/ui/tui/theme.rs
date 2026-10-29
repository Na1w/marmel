//! Semantic palette (colour *role* tokens), state glyphs and border/focus
//! styles — spec §7 (visual language).
//!
//! One source of truth for colour and glyph decisions: every pane component asks
//! this module for a **role token** (`frame_focus`, `state_running`, `role_tool`,
//! `scroll_unseen`, …) or a [`Glyph`], and never names a `Color` itself.
//!
//! (§F1…§F9 below are the accessibility findings of `docs/research_color_theme.md`;
//! each one is cited where the palette closes it.)
//!
//! # Design rules (normative)
//!
//! 1. **A token names a UI role, never a hue.** `state_running`, not `yellow()`.
//!    A call site therefore stays readable when a hue moves, and a hue can be
//!    re-used in a different region without lying about what it marks.
//! 2. **Named ANSI-16 colours only.** An RGB or 256-colour value would be a
//!    guess about the operator's profile. The capability layer below reads
//!    `NO_COLOR` and `TERM` to decide *whether to paint at all*; it deliberately
//!    does not negotiate a wider ramp (`COLORTERM` is ignored on purpose, see
//!    "Capability layer"). Every value below is a ratatui named colour, which the
//!    terminal profile maps. `palette_stays_on_the_named_ansi_16_ramp` fails if
//!    that ever changes.
//! 3. **Colour never carries meaning alone.** Every state also ships a glyph or a
//!    word ([`Glyph::symbol`] + [`Glyph::label`], the `●` [`FOCUS_BADGE`], verdict
//!    words, section labels). Hues separate *neighbours*, not *meanings*.
//! 4. **Warm = problem.** `Red` is failure/irreversible only; `LightRed` is
//!    "needs attention, nothing broke" only. Waiting/live/new/queued states are
//!    cool (`Blue`/`LightBlue`) or neutral (`Gray`/`White`) so a benign chip can
//!    never read as one lightness step from an error.
//! 5. **Band-aware.** The status row is painted over the `BAND` background, and a
//!    style with no `bg` inherits it: `text_muted` (DarkGray) on the band is
//!    invisible. Anything drawn inside the band must use a band slot that pairs an
//!    `fg` with `bg(BAND)` — the `band_slots_never_paint_the_band_colour_on_itself`
//!    test fails otherwise.
//!
//! # Capability layer (research §F9)
//!
//! §F9 recorded that the palette assumed a colour-capable terminal: there was no
//! `NO_COLOR`, `TERM`, `COLORTERM` or `COLORFGBG` handling anywhere, and
//! `TuiRenderer::init` only enables raw mode + the alt screen. `capability()` now
//! closes that gap **once per process** — a `OnceLock` environment read in the
//! same shape as the probe in `harness::sandbox::kernel_has_landlock`, so no
//! frame ever calls `getenv` — and every slot constructor funnels its style
//! through `degrade()`. A call site therefore never learns the decision exists:
//! the call-site routing pass still just asks for a role token.
//!
//! The rules, in order (pinned by `no_color_empty_does_not_disable_colour` and
//! `dumb_or_absent_term_degrades_to_monochrome`):
//!
//! 1. `NO_COLOR` set to any **non-empty** value → monochrome. That is the
//!    <https://no-color.org> convention, including its quirk that the value is
//!    never parsed: `NO_COLOR=0` and `NO_COLOR=false` disable colour too.
//!    `NO_COLOR=` (empty) is *not* "off" — the convention defines presence with
//!    content as the switch, so an empty value leaves colour on
//!    (`no_color_empty_does_not_disable_colour`).
//! 2. `TERM=dumb`, or `TERM` absent/empty → monochrome. A ratatui app still
//!    needs ANSI to draw anything, but `dumb`/unlabelled is a promise that the
//!    named colours are not mapped to anything readable, so nothing is painted.
//!    The palette also stops depending on background fills there: `band_surface`
//!    and `caret_line` drop `bg(BAND)` and the band chips drop their fill, so a
//!    `dumb` profile is never handed a fill it may not render.
//! 3. Anything else → the designed palette, byte-identical to before this layer
//!    existed.
//!
//! In monochrome a style keeps its **modifiers** (`BOLD`, `ITALIC`) and drops
//! both colour channels; glyphs, word labels, the `●` focus badge and every
//! other piece of text are untouched — rule 3 is exactly what makes the degrade
//! survivable, and `monochrome_strips_colour_from_every_slot_and_keeps_its_modifiers`
//! plus `monochrome_keeps_every_glyph_label_and_badge_text` pin it.
//!
//! **Deliberately not implemented:** `COLORFGBG` light-background detection and
//! `COLORTERM` capability upgrades. `COLORFGBG` is written by a minority of
//! emulators, absent for most, and has no reliable default, so "this is a light
//! background" inferred from it is a guess — and there is nothing safe to do with
//! that guess: the palette has no light-background fork (rule 2 keeps the ramp
//! named and profile-relative, and `text_primary` is fg-less precisely so it
//! follows whatever the profile calls ink). Acting on the signal could therefore
//! only re-arrange the warm/cool contract of rule 4 while changing no glyph, no
//! label and no redundancy — a fork invented on an unverifiable environment
//! variable, in exchange for nothing. If a light-profile palette is ever wanted
//! it should be an explicit operator setting, not a `COLORFGBG` guess.
//! `COLORTERM` is ignored for the same reason in the other direction: it can
//! only license a 256/truecolour ramp that rule 2 refuses to use.
//!
//! In `cfg(test)` builds the palette is **pinned** instead of read from the
//! ambient environment (`capability`, `forced_capability`): an assertion about a
//! colour must not depend on the `TERM` a test runner happened to inherit, and a
//! test must not mutate process globals to force one
//! (`docs/fix_notice_reply_flakiness.md` §7 — libtest runs the tests in one
//! process in parallel, so a test that reads or writes shared process state gets
//! charged another test's traffic). Tests set a thread-local profile with the
//! `Forced` guard; the real environment read is exercised through the pure
//! `capability_of` plus `capability_is_resolved_once_and_never_read_per_frame`.
//!
//! # Compatibility bridge (closed — read this before adding a colour)
//!
//! The bridge existed so the palette could be redesigned without touching a call
//! site: the 11 names the panes called — `frame_focus`, `frame_idle`,
//! `frame_alert`, `frame_confirm`, `chip`, `chip_active`, `chip_done`,
//! `chip_pending`, `text_primary`, `text_dim`, `error` — kept byte-identical
//! values while each delegated to the semantic token that owns it. So the palette
//! could be redesigned without touching a single call site, and nothing in that
//! pass changes what a rendered frame looks like **except** the two glyph badges
//! whose old slots were outright wrong: `⧗` (waiting) moved off `LightRed`, and
//! `⏸` (paused) off the neutral chip slot.
//!
//! **The bridge is closed.** Every call site now asks for a role token — status
//! band, transcript roles, plan/workers/header/composer/overlay/scrollbars/caret
//! are all routed — and the frozen aliases are deleted: `chip`, `chip_active`,
//! `chip_done`, `chip_pending`, `error`, `text_dim` and `frame_alert` no longer
//! exist. A pane that names one of them fails to compile, which is the point: the
//! role token is the only way to ask for a colour. The one legacy name still
//! exported is [`frame_confirm`], because `components/help.rs` still calls it; the
//! pass that routes that call site deletes it.
//!
//! Deleting a name does not move a value. The slots that inherited the frozen
//! values are pinned twice: [`legacy_slots_keep_their_frozen_values`] below, and
//! the §7.1 contract test in `src/ui/tui/tests.rs`.
//!
//! # Bridge slots: declared here, routed later
//!
//! `Cargo.toml` denies `dead_code`/`unused`, and `mod theme;` is private to
//! `ui::tui`, so a token with no production call site fails `cargo build`. Every
//! token that no production code calls **yet** therefore carries a scoped
//! `dead_code` allowance plus a `route at <file:line>` note. That attribute is a
//! TODO with a compile-time guarantee attached: the routing commit removes it (and
//! only it) when it adds the call site. Grepping this module for that allowance is
//! the migration checklist, and it comes back **empty**: every token here has a
//! production call site, so a token added later without one is exactly what the
//! grep is there to catch.
//!
//! The attribute points the other way once a **legacy alias** loses its last call
//! site: the routing pass has nothing left to route, and only the value-pinned
//! contract test in `tests.rs` still names the alias. Such an alias carries the
//! same attribute with a note saying so, which is what keeps `cargo build` honest
//! between the routing pass and the sweep that deletes the name — the state the
//! seven deleted aliases were in, and the state [`frame_confirm`] reaches once the
//! help overlay is routed.
//!
//! # Token contract
//!
//! | Token | Role | Colour | Must be used for / must not be used for |
//! |---|---|---|---|
//! | `frame_focus` | focused pane frame + title | Cyan | pane chrome only; never a content accent, never a modal frame |
//! | `frame_idle` | unfocused pane frame + title | DarkGray | pane chrome only |
//! | `frame_danger` | frame for an irreversible/blocking state | Red | armed abort, too-small notice; never a benign warning |
//! | `frame_caution` | frame for a confirm gate | Yellow | confirm overlays only; never pane focus |
//! | `frame_notice` | frame for an informational modal | LightBlue | help/keymap overlay; must not reuse `frame_focus` ("a modal is up" ≠ "focus moved") |
//! | `text_primary` | body text, composer draft, overlay body | *(no fg, no bg)* | all prose; must stay fg-less so it follows the terminal profile — never force `White` |
//! | `text_secondary` | values, ids, empty-state and first-run guidance | Gray | text a new operator must read; never inside the band (use `chip_muted`/`chip_value`) |
//! | `text_muted` | chrome labels, separators, gutters, scroll readouts | DarkGray | chrome **outside** the band; never on the band (invisible), never an empty state |
//! | `text_thought` | reasoning/thinking stream | DarkGray + ITALIC | thought prose only; always preceded by a label (`[Thinking]`), because a terminal without italics collapses it onto `text_muted` |
//! | `section_header` | in-pane section labels (`[Validator]`, `[Output]`, `[Logs]`) | Gray + BOLD | headers only; carries no state, so never Red because a section is named Validator |
//! | `selection` | the cursor/pointer marking the selected row | White + BOLD | selection only; must not reuse the running-state hue, which is a state |
//! | `band_surface` | the status row *surface* | bg DarkGray | the band itself; fg always comes from a band chip, never from `text_*` |
//! | `chip_neutral` | state-echo chip on the band | White on DarkGray | neutral readouts on the band; never a verdict (verdicts have their own slots) |
//! | `chip_muted` | the muted rung *inside* the band | Gray on DarkGray | separators/labels on the band; this is the band-safe twin of `text_muted` |
//! | `chip_value` | a number worth reading inside the band | White on DarkGray + BOLD | token counts / metrics on the band; `text_primary` has no bg and must not be used there |
//! | `state_running` | in flight: active task, active worker, `▶` | Yellow + BOLD | in-flight only; do not re-add `BOLD` at the call site, and do not use it for selection |
//! | `state_queued` | waiting: `⧗`, open plan line, queue rank, queued steers, validating | Blue | waiting only; never failure-adjacent, never `val N` |
//! | `state_paused` | parked stream, `⏸` | Gray | paused only; must stay distinct from queued and from the neutral chip |
//! | `state_done` | delivered/approved/checked, `✓` | Green | success only; never the product wordmark, never "live/following" |
//! | `state_failed` | failed/aborted/rejected, `✗`, sticky error text | Red + BOLD | failure only; never a section header, never a warning |
//! | `state_replan` | needs attention, nothing broke (`REPLAN`, `val N`) | LightRed | replan/validation attention only; never an open task, queue rank, scroll state or overflow marker |
//! | `verdict_ok` | verdict: mission complete | Green | `Verdict::Complete` |
//! | `verdict_reject` | verdict: withdrawn/revoked (not a failure) | Gray + BOLD | `Verdict::Revoked`; must never equal `verdict_fail` (both draw `✗`) and must not reuse the chrome chip |
//! | `verdict_fail` | verdict: failed | Red + BOLD | `Verdict::Failed` |
//! | `verdict_replan` | verdict: replan required | LightRed | `Verdict::Replan` |
//! | `role_user` | the operator's own lines | LightCyan + BOLD | `User:`/`User (Steer):`; off Cyan so it does not read as focus |
//! | `role_manager` | the manager's voice (incl. steer arbitrator, streamed steer) | LightYellow + BOLD | manager lines only; one slot for all three current variants |
//! | `role_status` | `[Status]` / `[CLI]` lines | Yellow | machine status prose; not the band, not in-flight state |
//! | `role_tool` | `[Tool Call]` / `[Tool Result]` lines | Magenta | tool lines only |
//! | `role_turn` | turn separator rules | DarkGray + BOLD | separators; free the success hue from running across the transcript |
//! | `scroll_live` | following the live edge (`● LIVE`) | LightBlue | follow-on chip; must not use the success hue |
//! | `scroll_unseen` | new output below the fold (`↓ N new`) | Blue | unseen count; never `LightRed` |
//! | `scroll_stale` | follow is off / viewport is stale | Gray | follow-off chip; neutral, it is the operator's own choice |
//! | `scrollbar_thumb` | pane scrollbar thumb | Gray | `Scrollbar::thumb_style` |
//! | `scrollbar_track` | pane scrollbar track | DarkGray | `Scrollbar::track_style`; must stay readable next to `scrollbar_thumb` |
//! | `brand` | the product wordmark | Cyan + BOLD | the wordmark only — it must not borrow the success hue, or "green" stops meaning done |
//! | `notice_ref` | arbitration/notice reference id | LightYellow | notice ids; must not borrow `frame_focus` |
//! | `notice_expired` | a notice past its lifetime | LightRed | expired notices; attention family, not failure |
//! | `caret_line` | the composer line holding the caret | bg DarkGray | caret affordance on a light profile; never a selection highlight |
//! | `focus badge` (`FOCUS_BADGE`) | colour-independent focus marker | — (a glyph) | every pane title, always beside `frame_focus` |
//! | `clip marker` (`ELLIPSIS`) | clipped run of text | — (a glyph) | truncation only |
//!
//! The frozen aliases are gone; the slots that own the values they carried are:
//! `frame_alert`→[`frame_danger`], `chip`→[`chip_neutral`],
//! `chip_active`→[`state_running`], `chip_done`→[`state_done`],
//! `chip_pending`→[`state_replan`], `text_dim`→[`text_muted`] outside the band /
//! [`chip_muted`] inside it, `error`→[`state_failed`]. The only legacy name still
//! exported is `frame_confirm`→[`frame_caution`];
//! `frame_focus`/`frame_idle`/`text_primary` already named a role and are
//! unchanged.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Badge marking the focused pane in its title, independent of colour (§7.3, G6).
pub const FOCUS_BADGE: char = '●';

/// Mark a run of text clipped to a rect (§7.4).
pub const ELLIPSIS: char = '…';

/// The one surface colour in the palette: the DarkGray band the status row is
/// painted on. Anything drawn over it must name its own `fg`.
const BAND: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// Terminal capability (research §F9) — resolved once, applied in the constructors
// ---------------------------------------------------------------------------

/// What the terminal may be trusted to paint. The palette has exactly two
/// states: the designed one, and one with no colour at all. There is no
/// 16/256/truecolour ladder — design rule 2 keeps the ramp named, so the only
/// thing worth detecting is *whether to use colour at all*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    /// Named ANSI-16 colours are emitted: the palette exactly as specified.
    Colour,
    /// No `fg` and no `bg` at all; modifiers, glyphs and words carry the signal.
    Monochrome,
}

/// The rule set as a pure function of the two environment variables the
/// convention defines. It takes the values rather than reading them because a
/// test that reads or writes process globals is charged another test's traffic
/// (`docs/fix_notice_reply_flakiness.md` §7), and the palette's rules must be
/// pinned without mutating the environment of the test binary.
///
/// Order matters: an explicit operator preference (`NO_COLOR`) beats a terminal
/// that claims capability.
fn capability_of(no_color: Option<&str>, term: Option<&str>) -> Capability {
    // no-color.org: the value is never parsed, only presence *with content* is
    // the switch. `NO_COLOR=` is therefore not "off", and `NO_COLOR=0` is.
    if no_color.is_some_and(|value| !value.is_empty()) {
        return Capability::Monochrome;
    }
    // An absent, empty or whitespace-only `TERM` does not mean "uncoloured", it
    // means "unlabelled": the only honest reading is that the named colours may
    // not be mapped to anything readable, so nothing is painted.
    match term.map(str::trim).filter(|value| !value.is_empty()) {
        None => Capability::Monochrome,
        Some(value) if value.eq_ignore_ascii_case("dumb") => Capability::Monochrome,
        Some(_) => Capability::Colour,
    }
}

/// The one environment read, done at most once per process — the same shape as
/// the `OnceLock` probe in `harness::sandbox::kernel_has_landlock`, so a frame
/// never pays for `getenv`.
fn resolved_capability() -> Capability {
    static CACHED: std::sync::OnceLock<Capability> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        #[cfg(test)]
        ENV_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        capability_of(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// What every slot constructor asks for. In a real build this is the cached
/// environment read; in `cfg(test)` the profile is **pinned** (to colour, unless
/// a test forces another through `forced_capability`) so an assertion about a
/// hue can never depend on the `TERM` the test runner inherited — and so the
/// value-pinned palette tests in `src/ui/tui/tests.rs` stay valid in a test
/// build no matter what the CI shell exports.
#[cfg(test)]
fn capability() -> Capability {
    forced_capability().unwrap_or(Capability::Colour)
}

#[cfg(not(test))]
fn capability() -> Capability {
    resolved_capability()
}

// Test seam: the profile the running test thread is asserting against.
// Thread-local on purpose — libtest runs the tests of one binary in parallel
// threads sharing one process, so a process-global override would leak into
// unrelated tests exactly like `std::env::set_var` does.
#[cfg(test)]
thread_local! {
    static FORCED_CAPABILITY: std::cell::Cell<Option<Capability>> =
        const { std::cell::Cell::new(None) };
}

/// Counts the real environment reads, so "cached, never per frame" is a pinned
/// contract rather than a claim.
#[cfg(test)]
static ENV_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn forced_capability() -> Option<Capability> {
    FORCED_CAPABILITY.with(|cell| cell.get())
}

/// Degrade one designed style to the terminal's capability: the colour channels
/// go, everything else stays — the modifiers are the informational half of the
/// signal (§F9, design rule 3). Applied **inside** the constructors below, never
/// at a call site, so the call-site routing pass needs no knowledge of it.
fn degrade(style: Style) -> Style {
    if capability() == Capability::Colour {
        return style;
    }
    Style::default()
        .add_modifier(style.add_modifier)
        .remove_modifier(style.sub_modifier)
}

/// Foreground-only slot (used over the terminal's default background).
fn fg(color: Color) -> Style {
    degrade(Style::default().fg(color))
}

/// Foreground slot with weight — used where the marker is a role, not a state.
fn fg_bold(color: Color) -> Style {
    degrade(Style::default().fg(color).add_modifier(Modifier::BOLD))
}

/// A chip drawn over the `BAND` colour: it always carries its own background,
/// otherwise it silently inherits the band and a dark `fg` disappears (§F1).
/// In monochrome it carries no fill either — the band itself is unpainted there,
/// so a `dumb` profile is never asked to render one.
fn band_chip(color: Color) -> Style {
    degrade(Style::default().fg(color).bg(BAND))
}

/// The one filled surface in the palette (the status band, the caret line).
/// Monochrome removes the fill rather than trusting a `dumb`/unknown profile to
/// render a background the design does not need.
fn surface() -> Style {
    degrade(Style::default().bg(BAND))
}

// ---------------------------------------------------------------------------
// Pane frames
// ---------------------------------------------------------------------------

/// Focused pane frame and focused pane title (keeps today's Cyan).
pub fn frame_focus() -> Style {
    fg(Color::Cyan)
}

/// Unfocused pane frame and unfocused pane title.
pub fn frame_idle() -> Style {
    fg(Color::DarkGray)
}

/// Frame for a state that must stop the operator: armed abort, the too-small
/// notice. Never for a benign warning.
pub fn frame_danger() -> Style {
    fg(Color::Red)
}

/// Frame for a confirm gate — the operator is about to answer a question.
pub fn frame_caution() -> Style {
    fg(Color::Yellow)
}

/// Frame for an informational modal (help/keymap). Distinct from
/// [`frame_focus`]: a modal being open is not a focus change.
pub fn frame_notice() -> Style {
    fg(Color::LightBlue)
}

/// Legacy name, frozen value. Use [`frame_caution`].
pub fn frame_confirm() -> Style {
    frame_caution()
}

// ---------------------------------------------------------------------------
// Text hierarchy
// ---------------------------------------------------------------------------

/// Body text: composer draft, plan prose, transcript body, overlay body. Must
/// stay fg-less so it inherits the terminal's own foreground (research §F2:
/// forcing `White` is unreadable on a light profile). It names no colour, so it
/// is the one slot that degrades to itself in monochrome (§F9).
pub fn text_primary() -> Style {
    Style::default()
}

/// The rung the operator still has to read: metric values, ids, empty states and
/// the first-run guidance (§F3 — the dimmest tier must not be the one carrying
/// the message a newcomer needs).
pub fn text_secondary() -> Style {
    fg(Color::Gray)
}

/// Chrome: labels, separators, gutters, scroll readouts — **outside** the band.
pub fn text_muted() -> Style {
    fg(Color::DarkGray)
}

/// The reasoning stream. Luminance step plus italics, so it survives a terminal
/// that drops the modifier; the block must still be introduced by a label.
pub fn text_thought() -> Style {
    fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
}

// ---------------------------------------------------------------------------
// Structural emphasis (no state hue)
// ---------------------------------------------------------------------------

/// In-pane section labels (`[Thinking]`, `[Validator]`, `[Output]`, `[Logs]`).
/// One neutral slot replaces the four unrelated hues these carry today.
pub fn section_header() -> Style {
    fg_bold(Color::Gray)
}

/// The pointer marking the selected roster row. Neutral weight: it must not
/// borrow the in-flight hue of the row it points at.
pub fn selection() -> Style {
    fg_bold(Color::White)
}

// ---------------------------------------------------------------------------
// The band: one surface, three chip fgs
// ---------------------------------------------------------------------------

/// The status row's surface. The row's own `fg` always comes from a band chip.
/// In monochrome the fill is dropped: the band degrades to "no fill anywhere"
/// rather than asking an unknown profile to paint DarkGray (§F9).
pub fn band_surface() -> Style {
    surface()
}

/// Neutral state-echo chip on the band.
pub fn chip_neutral() -> Style {
    band_chip(Color::White)
}

/// The muted rung *inside* the band: separators, secondary labels, worker names.
/// This is the band-safe rung for the `text_muted` role, which would be
/// DarkGray-on-DarkGray over [`band_surface`] (§F1).
pub fn chip_muted() -> Style {
    band_chip(Color::Gray)
}

/// A number on the band worth reading (token counts, metrics). `text_primary`
/// carries no `bg` and must not be used inside the band.
pub fn chip_value() -> Style {
    band_chip(Color::White).add_modifier(Modifier::BOLD)
}

// ---------------------------------------------------------------------------
// State slots (plan tasks and worker rows)
// ---------------------------------------------------------------------------

/// In flight: the active plan task, a live worker, the active task id (`▶`).
pub fn state_running() -> Style {
    fg_bold(Color::Yellow)
}

/// Waiting on a slot or on the review loop: `⧗`, an open plan line, a queue
/// rank, a queued steer. Cool, because nothing is wrong (§F4).
pub fn state_queued() -> Style {
    fg(Color::Blue)
}

/// Held/parked: a stream stopped behind the arbitrator (`⏸`).
pub fn state_paused() -> Style {
    fg(Color::Gray)
}

/// Delivered and accepted: `✓`, `[x]`, an approved worker.
pub fn state_done() -> Style {
    fg(Color::Green)
}

/// Did not deliver: `✗`, aborted, rejected, and the sticky error line.
pub fn state_failed() -> Style {
    fg_bold(Color::Red)
}

/// Needs attention without having failed: `REPLAN REQUIRED`, the `val N` round
/// counter. The only home of the warm-but-not-alarm hue (§F4).
pub fn state_replan() -> Style {
    fg(Color::LightRed)
}

// ---------------------------------------------------------------------------
// Verdict slots (four, and they must never collide)
// ---------------------------------------------------------------------------

/// Verdict: mission complete.
pub fn verdict_ok() -> Style {
    state_done()
}

/// Verdict: withdrawn/revoked. A decision was taken back — that is not a
/// failure, and it must not reuse the neutral chrome chip either.
pub fn verdict_reject() -> Style {
    fg_bold(Color::Gray)
}

/// Verdict: failed.
pub fn verdict_fail() -> Style {
    state_failed()
}

/// Verdict: replan required.
pub fn verdict_replan() -> Style {
    state_replan()
}

// ---------------------------------------------------------------------------
// Transcript voice/line-type roles
// ---------------------------------------------------------------------------

/// The operator's own lines (`User:`, `User (Steer):`). Off Cyan so a user line
/// cannot read as pane focus.
pub fn role_user() -> Style {
    fg_bold(Color::LightCyan)
}

/// The manager's voice: `Marmennill:`, `[Steer Arbitrator]`, and the streamed
/// steer sentence. Today these are three separate spellings of one role.
pub fn role_manager() -> Style {
    fg_bold(Color::LightYellow)
}

/// `[Status]` / `[CLI]` lines.
pub fn role_status() -> Style {
    fg(Color::Yellow)
}

/// `[Tool Call]` / `[Tool Result]` lines.
pub fn role_tool() -> Style {
    fg(Color::Magenta)
}

/// Turn separators — chrome, so the success hue is not painted across the
/// transcript.
pub fn role_turn() -> Style {
    fg_bold(Color::DarkGray)
}

// ---------------------------------------------------------------------------
// Scroll / follow state and the scrollbar
// ---------------------------------------------------------------------------

/// Following the live edge (`● LIVE`). Not the success hue: a green corner
/// currently reads "done" wherever else it appears (§F5).
pub fn scroll_live() -> Style {
    fg(Color::LightBlue)
}

/// Output arrived below the fold while the operator is scrolled back (`↓ N new`).
pub fn scroll_unseen() -> Style {
    fg(Color::Blue)
}

/// Follow is off: the viewport is deliberately stale — the operator's own choice,
/// so neutral rather than alarming.
pub fn scroll_stale() -> Style {
    fg(Color::Gray)
}

/// Scrollbar thumb.
pub fn scrollbar_thumb() -> Style {
    fg(Color::Gray)
}

/// Scrollbar track.
pub fn scrollbar_track() -> Style {
    fg(Color::DarkGray)
}

// ---------------------------------------------------------------------------
// Accents that are not states
// ---------------------------------------------------------------------------

/// The product wordmark. Kept out of the success hue on purpose: if the logo is
/// green, a green chip stops meaning "done" (§F5).
pub fn brand() -> Style {
    fg_bold(Color::Cyan)
}

/// A steering/arbitration notice's reference id. Must not borrow pane focus.
pub fn notice_ref() -> Style {
    fg(Color::LightYellow)
}

/// A notice that ran past its lifetime without an answer.
pub fn notice_expired() -> Style {
    fg(Color::LightRed)
}

/// The composer line that holds the caret — the only caret affordance on a light
/// profile, where a default-coloured caret can vanish. It is a fill, so it is
/// the one slot that simply disappears in monochrome: the caret itself is still
/// drawn by the terminal, and nothing else in the palette relies on this fill.
pub fn caret_line() -> Style {
    surface()
}

// ---------------------------------------------------------------------------
// Glyph vocabulary (the colour-independent half of the design)
// ---------------------------------------------------------------------------

/// The five state glyphs of §7.2. They replace the prose-only
/// `(Active | Paused | Queued n/m | Idle)` labels, but [`Glyph::label`] is kept
/// beside the glyph whenever the rect has room, so a terminal without these
/// codepoints still reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// `✓` done / completed / idle.
    Done,
    /// `▶` running / active.
    Active,
    /// `⧗` queued or validating.
    Pending,
    /// `⏸` paused.
    Paused,
    /// `✗` failed / aborted.
    Failed,
}

impl Glyph {
    /// The glyph itself. Measured with `unicode-width` by the caller that clips
    /// it (spec §7.2); never assumed to be one cell.
    pub const fn symbol(self) -> char {
        match self {
            Self::Done => '✓',
            Self::Active => '▶',
            Self::Pending => '⧗',
            Self::Paused => '⏸',
            Self::Failed => '✗',
        }
    }

    /// The word label kept next to the glyph.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Active => "running",
            Self::Pending => "queued",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }

    /// The palette slot this glyph is drawn with. Each state owns exactly one
    /// slot, and no two states share a hue: that is what makes the glyph
    /// redundant rather than load-bearing.
    pub fn style(self) -> Style {
        match self {
            Self::Done => state_done(),
            Self::Active => state_running(),
            Self::Pending => state_queued(),
            Self::Paused => state_paused(),
            Self::Failed => state_failed(),
        }
    }

    /// Bare glyph as a styled span.
    pub fn badge(self) -> Span<'static> {
        Span::styled(self.symbol().to_string(), self.style())
    }

    /// Glyph plus word label as a styled span.
    pub fn tagged(self) -> Span<'static> {
        Span::styled(format!("{} {}", self.symbol(), self.label()), self.style())
    }
}

/// Glyph for a plan task line: checked-off tasks read `✓`, the task in flight
/// reads `▶`, everything else is pending `⧗` (spec §7.2).
pub const fn task_glyph(completed: bool, active: bool) -> Glyph {
    if completed {
        Glyph::Done
    } else if active {
        Glyph::Active
    } else {
        Glyph::Pending
    }
}

/// Glyph for a specialist roster row, derived from the registry-projected row
/// state (`is_active` / `paused` / `queue`, spec §8.3). A row with nothing in
/// flight reads `✓` (idle = finished, never `▶`).
///
/// This is the label-free half of the rule: with only liveness projected, "not
/// running" is all a row can honestly say. [`worker_state_glyph`] is the entry
/// point a caller uses once the registry's own `status_label` is on the row
/// (plumbing delta D1, design §9.3), because liveness alone cannot tell an aborted
/// worker from an approved one.
pub const fn worker_glyph(is_active: bool, paused: bool, queued: bool) -> Glyph {
    if paused {
        Glyph::Paused
    } else if queued {
        Glyph::Pending
    } else if is_active {
        Glyph::Active
    } else {
        Glyph::Done
    }
}

/// Terminal status labels that mean "this worker did not deliver" (§7.2 `✗`).
/// Matched against the leading word of `WorkerEntry::status_label`, the vocabulary
/// the runner and the review loop actually write: `Aborted`, `Rejected`, `Failed`,
/// `Replan Required (…)`.
const FAILED_STATUS_LABELS: [&str; 4] = ["aborted", "failed", "rejected", "replan"];
/// Labels that mean "still working, held by the review loop" (§7.2 `⧗`, which the
/// spec spells "queued **or validating**").
const PENDING_STATUS_LABELS: [&str; 3] = ["revising", "validating", "queued"];
/// Labels that mean the worker delivered and was accepted (§7.2 `✓`).
const DONE_STATUS_LABELS: [&str; 2] = ["approved", "completed"];

/// The §7.2 glyph for a worker whose registry label is projected (D1).
///
/// The slot state comes first — a stream parked behind the arbitrator, or a
/// delegation waiting for a backend slot, is in those states regardless of what
/// its last label said. Then the label decides a **terminal** state, which is the
/// whole point of D1: `is_active == false` says the stream closed, and only the
/// label says whether it closed approved or aborted. An aborted or
/// validator-rejected worker therefore reads `✗ failed` and never `✓ done`.
///
/// A label the vocabulary does not know (`In Progress`, `Running`) decides
/// nothing, and the caller falls back to [`worker_glyph`]: guessing a terminal state
/// from prose would be inventing a state the registry never reported.
pub fn worker_state_glyph(
    is_active: bool,
    paused: bool,
    queued: bool,
    status_label: Option<&str>,
) -> Glyph {
    if paused || queued {
        return worker_glyph(is_active, paused, queued);
    }
    if let Some(label) = status_label
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        // Only the leading word decides: `Replan Required (…)` is a replan verdict,
        // and a label that merely mentions a verdict word later is not one.
        let head = label
            .split([' ', '-', '(', ':'])
            .next()
            .unwrap_or(label)
            .trim()
            .to_ascii_lowercase();
        if FAILED_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Failed;
        }
        if PENDING_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Pending;
        }
        if DONE_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Done;
        }
    }
    worker_glyph(is_active, paused, queued)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every palette slot, by name. The semantic slots are listed here whether or
    /// not a call site routes them yet: a semantic separation must be pinned
    /// before the call sites exist, otherwise the routing pass can quietly merge
    /// two roles.
    fn slots() -> Vec<(&'static str, Style)> {
        vec![
            ("frame_focus", frame_focus()),
            ("frame_idle", frame_idle()),
            ("frame_danger", frame_danger()),
            ("frame_caution", frame_caution()),
            ("frame_notice", frame_notice()),
            ("text_primary", text_primary()),
            ("text_secondary", text_secondary()),
            ("text_muted", text_muted()),
            ("text_thought", text_thought()),
            ("section_header", section_header()),
            ("selection", selection()),
            ("band_surface", band_surface()),
            ("chip_neutral", chip_neutral()),
            ("chip_muted", chip_muted()),
            ("chip_value", chip_value()),
            ("state_running", state_running()),
            ("state_queued", state_queued()),
            ("state_paused", state_paused()),
            ("state_done", state_done()),
            ("state_failed", state_failed()),
            ("state_replan", state_replan()),
            ("verdict_ok", verdict_ok()),
            ("verdict_reject", verdict_reject()),
            ("verdict_fail", verdict_fail()),
            ("verdict_replan", verdict_replan()),
            ("role_user", role_user()),
            ("role_manager", role_manager()),
            ("role_status", role_status()),
            ("role_tool", role_tool()),
            ("role_turn", role_turn()),
            ("scroll_live", scroll_live()),
            ("scroll_unseen", scroll_unseen()),
            ("scroll_stale", scroll_stale()),
            ("scrollbar_thumb", scrollbar_thumb()),
            ("scrollbar_track", scrollbar_track()),
            ("brand", brand()),
            ("notice_ref", notice_ref()),
            ("notice_expired", notice_expired()),
            ("caret_line", caret_line()),
        ]
    }

    /// Every slot a pane may reach for today (the semantic slots, plus the one
    /// legacy name the bridge still exports), so a new token cannot silently
    /// duplicate an existing one.
    fn style_of(name: &str) -> Style {
        slots()
            .into_iter()
            .find(|(slot, _)| *slot == name)
            .map(|(_, style)| style)
            .unwrap_or_else(|| panic!("`{name}` is not a palette slot"))
    }

    fn fg_of(name: &str) -> Option<Color> {
        style_of(name).fg
    }

    /// The legacy names the bridge still exports (§Compatibility bridge). After
    /// the final sweep the frozen aliases are deleted, so this list is the one
    /// name with a live call site left behind it. It is tested separately from
    /// `slots()` because it must degrade exactly like the semantic slot it
    /// delegates to — the call site that has not been routed yet may not be the
    /// one that keeps the capability contract.
    fn aliases() -> Vec<(&'static str, Style)> {
        vec![("frame_confirm", frame_confirm())]
    }

    /// The five state glyphs of §7.2.
    const ALL_GLYPHS: [Glyph; 5] = [
        Glyph::Done,
        Glyph::Active,
        Glyph::Pending,
        Glyph::Paused,
        Glyph::Failed,
    ];

    /// A style may never put a colour on the same colour: that is invisible
    /// text, not a low-contrast one. Holds in every capability tier.
    fn assert_no_fg_on_same_bg(name: &str, style: Style) {
        if let (Some(fg), Some(bg)) = (style.fg, style.bg) {
            assert_ne!(fg, bg, "`{name}` paints {fg:?} on {bg:?}: invisible text");
        }
    }

    /// Test seam: pin this thread's [`Capability`] for one test and put it back
    /// on drop. Thread-local + RAII because libtest runs the tests of one binary
    /// in parallel in one process: a process-global override (or
    /// `std::env::set_var`) would leak the profile into unrelated tests, which is
    /// the exact hazard `docs/fix_notice_reply_flakiness.md` §7 documents.
    struct Forced {
        previous: Option<Capability>,
    }

    impl Forced {
        fn colour() -> Self {
            Self::set(Capability::Colour)
        }

        fn monochrome() -> Self {
            Self::set(Capability::Monochrome)
        }

        fn set(capability: Capability) -> Self {
            Self {
                previous: FORCED_CAPABILITY.with(|cell| cell.replace(Some(capability))),
            }
        }
    }

    impl Drop for Forced {
        fn drop(&mut self) {
            FORCED_CAPABILITY.with(|cell| cell.set(self.previous));
        }
    }

    fn assert_pairwise_distinct(names: &[&str]) {
        for (i, a) in names.iter().enumerate() {
            for b in names.iter().skip(i + 1) {
                assert_ne!(
                    fg_of(a),
                    fg_of(b),
                    "`{a}` and `{b}` would render the same colour"
                );
            }
        }
    }

    #[test]
    fn palette_stays_on_the_named_ansi_16_ramp() {
        // The capability layer decides *whether* to paint, never with what: no
        // 256/truecolour negotiation exists on purpose, so a truecolor or indexed
        // value would still be a guess about the operator's terminal profile.
        for (name, style) in slots() {
            for (channel, color) in [("fg", style.fg), ("bg", style.bg)] {
                match color {
                    None | Some(Color::Reset) => {}
                    Some(Color::Rgb(..)) | Some(Color::Indexed(_)) => {
                        panic!("`{name}.{channel}` leaves the named ANSI-16 ramp: {color:?}");
                    }
                    Some(_) => {}
                }
            }
        }
    }

    #[test]
    fn warm_hues_are_reserved_for_failure_and_attention() {
        // §F4: a benign chip must never sit one lightness step from an error.
        let benign = [
            "state_queued",
            "state_paused",
            "state_done",
            "state_running",
            "chip_neutral",
            "chip_muted",
            "chip_value",
            "scroll_live",
            "scroll_unseen",
            "scroll_stale",
            "scrollbar_thumb",
            "scrollbar_track",
            "text_primary",
            "text_secondary",
            "text_muted",
            "text_thought",
            "section_header",
            "selection",
            "frame_idle",
            "frame_caution",
            "frame_notice",
            "frame_focus",
            "role_user",
            "role_status",
            "role_tool",
            "role_turn",
            "brand",
            "notice_ref",
        ];
        for name in benign {
            assert_ne!(
                fg_of(name),
                Some(Color::Red),
                "`{name}` would read as a failure"
            );
            assert_ne!(
                fg_of(name),
                Some(Color::LightRed),
                "`{name}` would read as an error one step away"
            );
        }
        assert_eq!(fg_of("state_failed"), Some(Color::Red));
        assert_eq!(fg_of("state_replan"), Some(Color::LightRed));
        assert_eq!(fg_of("frame_danger"), Some(Color::Red));
        assert_eq!(fg_of("frame_caution"), Some(Color::Yellow));
    }

    #[test]
    fn every_state_slot_is_its_own_colour() {
        assert_pairwise_distinct(&[
            "state_running",
            "state_queued",
            "state_paused",
            "state_done",
            "state_failed",
        ]);
        // The two slots that used to be coincidental: paused was the neutral
        // chrome chip and queued wore the failure-adjacent hue.
        assert_ne!(fg_of("state_paused"), fg_of("chip_neutral"));
        assert_ne!(fg_of("state_queued"), fg_of("state_replan"));
        assert_ne!(fg_of("state_done"), fg_of("state_running"));
    }

    #[test]
    fn the_four_verdict_slots_never_collide() {
        // `✗` is shared by Revoked and Failed, so the slots themselves must differ
        // (mirrors the guard in `components/transcript.rs`).
        assert_pairwise_distinct(&[
            "verdict_ok",
            "verdict_reject",
            "verdict_fail",
            "verdict_replan",
        ]);
        // Revoked used to be drawn with the chrome chip; a verdict may not wear
        // chrome.
        assert_ne!(fg_of("verdict_reject"), fg_of("chip_neutral"));
        assert_ne!(fg_of("verdict_reject"), fg_of("state_failed"));
    }

    #[test]
    fn frames_are_pairwise_distinguishable() {
        assert_pairwise_distinct(&[
            "frame_focus",
            "frame_idle",
            "frame_danger",
            "frame_caution",
            "frame_notice",
        ]);
    }

    #[test]
    fn transcript_roles_are_pairwise_distinguishable() {
        assert_pairwise_distinct(&[
            "role_user",
            "role_manager",
            "role_status",
            "role_tool",
            "role_turn",
            "text_primary",
        ]);
        // The manager's voice and the in-flight state used to be a lightness step
        // apart; they may not share a hue either.
        assert_ne!(fg_of("role_manager"), fg_of("state_running"));
        // A user line must not read as pane focus.
        assert_ne!(fg_of("role_user"), fg_of("frame_focus"));
        // A notice id must not borrow pane focus either.
        assert_ne!(fg_of("notice_ref"), fg_of("frame_focus"));
    }

    #[test]
    fn follow_state_slots_are_distinguishable() {
        assert_pairwise_distinct(&["scroll_live", "scroll_unseen", "scroll_stale"]);
        // "Live" wore the success hue; a green corner must keep meaning done.
        assert_ne!(fg_of("scroll_live"), fg_of("state_done"));
        assert_ne!(fg_of("scroll_unseen"), fg_of("state_replan"));
        assert_ne!(fg_of("scroll_stale"), fg_of("state_failed"));
        // The wordmark wore the success hue too.
        assert_ne!(fg_of("brand"), fg_of("state_done"));
        assert_pairwise_distinct(&["scrollbar_thumb", "scrollbar_track"]);
    }

    #[test]
    fn band_slots_never_paint_the_band_colour_on_itself() {
        // §F1: `text_muted` over the `band_surface` fill is DarkGray on DarkGray,
        // i.e. invisible text. Everything inside the band names its own `fg`.
        for name in ["chip_neutral", "chip_muted", "chip_value"] {
            let style = slots()
                .into_iter()
                .find(|(slot, _)| *slot == name)
                .map(|(_, style)| style)
                .unwrap();
            assert_eq!(style.bg, Some(BAND), "`{name}` must carry the band");
            assert_ne!(
                style.fg,
                Some(BAND),
                "`{name}` would be invisible on the band"
            );
        }
        assert_eq!(band_surface().bg, Some(BAND));
        assert_eq!(band_surface().fg, None, "the surface owns no text colour");
        // The reason the band needs its own muted rung.
        assert_eq!(text_muted().bg, None);
        assert_eq!(text_muted().fg, Some(BAND));
        assert_eq!(chip_muted().fg, text_secondary().fg);
    }

    #[test]
    fn body_text_inherits_the_terminal_profile() {
        // §F2: forcing a foreground for body text is unreadable on a light
        // profile, and `text_primary() == Style::default()` is also asserted by
        // the value-pinning test in `tests.rs`.
        assert_eq!(text_primary(), Style::default());
        assert_eq!(text_primary().fg, None);
        assert_eq!(text_primary().bg, None);
        // The three rungs below body text are distinct and never `White`, which
        // would vanish on a light profile.
        assert_ne!(text_secondary().fg, text_muted().fg);
        assert_ne!(text_thought().fg, text_secondary().fg);
        assert_ne!(text_secondary().fg, Some(Color::White));
        assert_ne!(text_muted().fg, Some(Color::White));
        assert!(
            text_thought().add_modifier.contains(Modifier::ITALIC),
            "thought carries a modifier step as well as a luminance step"
        );
        assert_ne!(
            text_thought(),
            text_muted(),
            "thought is not just dim chrome"
        );
    }

    #[test]
    fn selection_and_section_headers_carry_no_state_hue() {
        // A pointer or a heading must never imply a lifecycle state. Neutrals
        // (Gray/White/DarkGray) are fine; the five state hues are not.
        let state_hues = [
            state_running().fg,
            state_queued().fg,
            state_done().fg,
            state_failed().fg,
            state_replan().fg,
        ];
        for name in ["selection", "section_header", "brand", "notice_ref"] {
            let color = fg_of(name);
            assert!(
                !state_hues.contains(&color),
                "`{name}` ({color:?}) would imply a lifecycle state"
            );
        }
        assert_pairwise_distinct(&["selection", "section_header"]);
    }

    #[test]
    fn glyph_slots_are_pairwise_distinct_and_redundant() {
        let glyphs = [
            Glyph::Done,
            Glyph::Active,
            Glyph::Pending,
            Glyph::Paused,
            Glyph::Failed,
        ];
        for (i, a) in glyphs.iter().enumerate() {
            for b in glyphs.iter().skip(i + 1) {
                assert_ne!(a.style().fg, b.style().fg, "{a:?} and {b:?} share a hue");
                assert_ne!(a.label(), b.label(), "{a:?} and {b:?} share a word");
                assert_ne!(a.symbol(), b.symbol(), "{a:?} and {b:?} share a glyph");
            }
        }
        // The state slot is the palette's, not a glyph-local accident.
        assert_eq!(Glyph::Pending.style().fg, fg_of("state_queued"));
        assert_eq!(Glyph::Paused.style().fg, fg_of("state_paused"));
        assert_eq!(Glyph::Failed.style().fg, fg_of("state_failed"));
        assert_eq!(Glyph::Done.style().fg, fg_of("state_done"));
        assert_eq!(Glyph::Active.style().fg, fg_of("state_running"));
    }

    #[test]
    fn legacy_slots_keep_their_frozen_values() {
        // The bridge promise, restated after the sweep: the frozen aliases are
        // deleted, but every value they carried still belongs to the same slot, so
        // routing the call sites moved nothing in a rendered frame. `tests.rs`
        // pins the same values from the §7.1 contract.
        //
        // The one legacy name still exported keeps delegating.
        assert_eq!(frame_confirm(), frame_caution());

        // What each deleted alias used to return, now owned by its semantic slot:
        //   `frame_alert`   -> frame_danger
        assert_eq!(frame_danger().fg, Some(Color::Red));
        //   `frame_confirm` -> frame_caution
        assert_eq!(frame_caution().fg, Some(Color::Yellow));
        //   `chip_active`   -> state_running
        assert_eq!(state_running().fg, Some(Color::Yellow));
        assert!(state_running().add_modifier.contains(Modifier::BOLD));
        //   `chip`          -> chip_neutral (over band_surface)
        assert_eq!(chip_neutral().fg, Some(Color::White));
        assert_eq!(chip_neutral().bg, Some(BAND));
        //   `chip_done`     -> state_done
        assert_eq!(state_done().fg, Some(Color::Green));
        //   `chip_pending`  -> state_replan
        assert_eq!(state_replan().fg, Some(Color::LightRed));
        //   `error`         -> state_failed
        assert_eq!(state_failed().fg, Some(Color::Red));
        assert!(state_failed().add_modifier.contains(Modifier::BOLD));
        //   `text_dim`      -> text_muted outside the band, chip_muted inside it
        assert_eq!(text_muted().fg, Some(Color::DarkGray));
        assert_eq!(chip_muted().fg, Some(Color::Gray));
        //   the names that already named a role are unchanged
        assert_eq!(frame_focus().fg, Some(Color::Cyan));
        assert_eq!(frame_idle().fg, Some(Color::DarkGray));
        assert_eq!(text_primary(), Style::default());

        // …and the slots that inherited those values stay separated from each
        // other, because the call sites that use them are live today.
        assert_ne!(state_replan().fg, state_failed().fg);
        assert_ne!(state_done().fg, state_running().fg);
        assert_ne!(frame_danger().fg, frame_caution().fg);
        assert_ne!(frame_focus().fg, frame_idle().fg);
        assert_ne!(text_muted().fg, chip_muted().fg);
    }

    // -----------------------------------------------------------------------
    // Capability layer (research §F9): NO_COLOR / TERM, cached once
    // -----------------------------------------------------------------------

    #[test]
    fn no_color_empty_does_not_disable_colour() {
        // no-color.org: presence *with content* is the switch, so an empty value
        // is not "off" and colour stays on.
        for term in ["xterm-256color", "tmux-256color", "linux"] {
            assert_eq!(
                capability_of(Some(""), Some(term)),
                Capability::Colour,
                "`NO_COLOR=` must not disable colour (TERM={term})"
            );
        }
        // The value is never parsed, so the "false-ish" spellings still disable.
        for value in ["1", "0", "false", "auto", " "] {
            assert_eq!(
                capability_of(Some(value), Some("xterm-256color")),
                Capability::Monochrome,
                "`NO_COLOR={value:?}` disables colour by convention"
            );
        }
        // An explicit preference beats a terminal that claims capability.
        assert_eq!(
            capability_of(Some("1"), Some("dumb")),
            Capability::Monochrome,
            "both signals point the same way"
        );
        assert_eq!(
            capability_of(Some("1"), None),
            Capability::Monochrome,
            "`NO_COLOR` alone is enough"
        );
    }

    #[test]
    fn dumb_or_absent_term_degrades_to_monochrome() {
        for term in [None, Some(""), Some("   "), Some("dumb"), Some("Dumb")] {
            assert_eq!(
                capability_of(None, term),
                Capability::Monochrome,
                "TERM={term:?} is not a colour contract"
            );
        }
        for term in [
            "xterm",
            "xterm-256color",
            "tmux-256color",
            "screen-256color",
            "vt100",
            "linux",
        ] {
            assert_eq!(
                capability_of(None, Some(term)),
                Capability::Colour,
                "TERM={term} is a labelled profile"
            );
        }
        // With no `NO_COLOR` and a capable `TERM`, the designed palette is used.
        assert_eq!(
            capability_of(None, Some("xterm-256color")),
            Capability::Colour
        );
    }

    #[test]
    fn capability_is_resolved_once_and_never_read_per_frame() {
        // The read is a `OnceLock` initialisation, not per-frame `getenv`
        // (mirrors `sandbox::kernel_has_landlock`).
        let first = resolved_capability();
        let reads = ENV_READS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            reads <= 1,
            "the environment was read {reads} times: the detection is not cached"
        );
        for _ in 0..1_000 {
            assert_eq!(resolved_capability(), first, "the answer drifted");
        }
        assert_eq!(
            ENV_READS.load(std::sync::atomic::Ordering::Relaxed),
            reads,
            "1 000 palette lookups re-read the environment"
        );
    }

    #[test]
    fn the_test_build_pins_the_profile_and_the_forced_guard_nests() {
        // An assertion about a hue may never depend on the `TERM` the runner
        // inherited, and a monochrome test may not leak its profile into the
        // next test that runs in this process.
        assert_eq!(capability(), Capability::Colour);
        assert_eq!(frame_focus().fg, Some(Color::Cyan));
        {
            let _monochrome = Forced::monochrome();
            assert_eq!(capability(), Capability::Monochrome);
            assert_eq!(frame_focus().fg, None);
            {
                let _inner = Forced::colour();
                assert_eq!(frame_focus().fg, Some(Color::Cyan));
            }
            assert_eq!(
                capability(),
                Capability::Monochrome,
                "the inner guard restored, it did not overwrite"
            );
        }
        assert_eq!(capability(), Capability::Colour, "the guard restored");
        assert_eq!(frame_focus().fg, Some(Color::Cyan));
    }

    #[test]
    fn monochrome_strips_colour_from_every_slot_and_keeps_its_modifiers() {
        // Snapshot the colour-capable palette first: it is the baseline the
        // degrade is measured against, and it must stay byte-identical (§F9).
        let designed = slots();
        let aliased = aliases();
        let _monochrome = Forced::monochrome();
        let degraded = slots();
        let degraded_aliases = aliases();

        let pairs = designed
            .iter()
            .zip(degraded.iter())
            .chain(aliased.iter().zip(degraded_aliases.iter()));
        for ((name, want), (same, style)) in pairs {
            assert_eq!(name, same, "the slot list changed under my feet");
            assert_eq!(style.fg, None, "`{name}` still names a foreground");
            assert_eq!(style.bg, None, "`{name}` still names a background");
            assert_eq!(
                style.add_modifier, want.add_modifier,
                "`{name}` lost a modifier the colour palette carries"
            );
            assert_eq!(
                style.sub_modifier, want.sub_modifier,
                "`{name}` changed a modifier removal"
            );
        }
        // The informational modifiers survive, because colour is not allowed to
        // be the only signal in the first place (design rule 3).
        assert!(state_failed().add_modifier.contains(Modifier::BOLD));
        assert!(state_running().add_modifier.contains(Modifier::BOLD));
        assert!(chip_value().add_modifier.contains(Modifier::BOLD));
        assert!(selection().add_modifier.contains(Modifier::BOLD));
        assert!(text_thought().add_modifier.contains(Modifier::ITALIC));
        // Degrading is not flattening: the tiers that carry weight stay apart.
        assert_ne!(
            text_thought(),
            text_muted(),
            "thought collapsed into chrome"
        );
        assert_ne!(
            state_failed(),
            text_muted(),
            "failure collapsed into chrome"
        );
        // The one legacy name the bridge still exports degrades exactly like the
        // slot it delegates to.
        assert_eq!(frame_confirm(), frame_caution());
    }

    #[test]
    fn monochrome_keeps_every_glyph_label_and_badge_text() {
        let designed: Vec<(Glyph, char, &'static str, Style)> = ALL_GLYPHS
            .iter()
            .map(|glyph| (*glyph, glyph.symbol(), glyph.label(), glyph.style()))
            .collect();
        let _monochrome = Forced::monochrome();

        for (glyph, symbol, label, want) in designed {
            assert_eq!(glyph.symbol(), symbol, "{glyph:?} lost its glyph");
            assert_eq!(glyph.label(), label, "{glyph:?} lost its word label");
            let style = glyph.style();
            assert_eq!(style.fg, None, "{glyph:?} still names a foreground");
            assert_eq!(style.bg, None, "{glyph:?} still names a background");
            assert_eq!(style.add_modifier, want.add_modifier);
            // The spans the panes actually render keep their text verbatim.
            assert_eq!(glyph.badge().content, symbol.to_string());
            assert_eq!(glyph.tagged().content, format!("{symbol} {label}"));
            assert_eq!(glyph.badge().style.fg, None);
            assert_eq!(glyph.tagged().style.bg, None);
        }
        // The colour-independent markers are untouched by definition.
        assert_eq!(FOCUS_BADGE, '●');
        assert_eq!(ELLIPSIS, '…');
        // Glyph *selection* is a colour-free decision and does not move.
        assert_eq!(task_glyph(true, false), Glyph::Done);
        assert_eq!(task_glyph(false, true), Glyph::Active);
        assert_eq!(task_glyph(false, false), Glyph::Pending);
        assert_eq!(worker_glyph(true, true, false), Glyph::Paused);
        assert_eq!(
            worker_state_glyph(false, false, false, Some("Aborted")),
            Glyph::Failed
        );
    }

    #[test]
    fn no_slot_paints_a_colour_on_the_same_colour() {
        // §F1 generalized: invisible text is a bug in either capability tier.
        for profile in [Capability::Colour, Capability::Monochrome] {
            let _forced = Forced::set(profile);
            for (name, style) in slots().into_iter().chain(aliases()) {
                assert_no_fg_on_same_bg(name, style);
            }
        }
    }

    #[test]
    fn monochrome_band_slots_never_paint_a_fill_or_collapse_fg_onto_bg() {
        let _monochrome = Forced::monochrome();
        for name in [
            "band_surface",
            "chip_neutral",
            "chip_muted",
            "chip_value",
            "caret_line",
        ] {
            let style = style_of(name);
            assert_eq!(
                style.bg, None,
                "`{name}` depends on a background fill a dumb profile may not render"
            );
            assert_eq!(style.fg, None, "`{name}` still names a colour");
            assert_no_fg_on_same_bg(name, style);
            // Belt and braces: the band colour cannot reappear on either channel.
            assert_ne!(
                style.fg,
                Some(BAND),
                "`{name}` would be invisible on the band"
            );
            assert_ne!(style.bg, Some(BAND), "`{name}` still fills the band");
        }
        // The band-aware contract of design rule 5 is only suspended, not lost:
        // with colour available the same slots pair their `fg` with `bg(BAND)`.
        let _colour = Forced::colour();
        assert_eq!(band_surface().bg, Some(BAND));
        assert_eq!(caret_line().bg, Some(BAND));
        for name in ["chip_neutral", "chip_muted", "chip_value"] {
            assert_eq!(
                style_of(name).bg,
                Some(BAND),
                "`{name}` must carry the band"
            );
            assert_no_fg_on_same_bg(name, style_of(name));
        }
    }

    #[test]
    fn the_capability_layer_is_the_identity_when_colour_is_available() {
        // The acceptance claim for this pass, stated directly: on a colour-capable
        // terminal `degrade` changes nothing, so no slot value moved.
        let _colour = Forced::colour();
        assert_eq!(
            capability_of(None, Some("xterm-256color")),
            Capability::Colour
        );
        for (name, style) in slots().into_iter().chain(aliases()) {
            assert_eq!(degrade(style), style, "`{name}` changed value");
        }
        for glyph in ALL_GLYPHS {
            assert_eq!(degrade(glyph.style()), glyph.style(), "{glyph:?} changed");
        }
    }
}
