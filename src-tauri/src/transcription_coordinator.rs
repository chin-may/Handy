use crate::actions::ACTION_MAP;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::RecordingMode;
use log::{debug, error, warn};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const DEBOUNCE: Duration = Duration::from_millis(30);
const RELEASE_GRACE: Duration = Duration::from_millis(50);
/// A release at this threshold is a held recording; shorter releases latch.
const TAP_OR_HOLD_THRESHOLD: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttAction {
    Passthrough,
    DeferRelease,
    CancelRelease,
}

struct PendingRelease {
    binding_id: String,
    hotkey_string: String,
    deadline: Instant,
    recording_mode: RecordingMode,
    press_started_at: Instant,
    released_at: Instant,
}

/// Commands processed sequentially by the coordinator thread.
enum Command {
    Input {
        binding_id: String,
        hotkey_string: String,
        is_pressed: bool,
        recording_mode: RecordingMode,
    },
    Cancel {
        recording_was_active: bool,
    },
    ShortcutsSuspended,
    ProcessingFinished,
}

/// Pipeline lifecycle, owned exclusively by the coordinator thread.
enum Stage {
    Idle,
    Recording {
        binding_id: String,
        recording_mode: RecordingMode,
    },
    Processing,
}

fn classify_release_event(
    pending_release_binding: Option<&str>,
    is_pressed: bool,
    recording_mode: RecordingMode,
    binding_id: &str,
    recording_binding: Option<&str>,
) -> PttAction {
    if recording_mode == RecordingMode::Toggle {
        return PttAction::Passthrough;
    }

    if is_pressed {
        if pending_release_binding == Some(binding_id) {
            PttAction::CancelRelease
        } else {
            PttAction::Passthrough
        }
    } else if recording_binding == Some(binding_id) && pending_release_binding.is_none() {
        PttAction::DeferRelease
    } else {
        PttAction::Passthrough
    }
}

fn is_short_tap(press_started_at: Instant, released_at: Instant) -> bool {
    released_at.duration_since(press_started_at) < TAP_OR_HOLD_THRESHOLD
}

fn effective_recording_mode(
    input_mode: RecordingMode,
    active_recording_mode: Option<RecordingMode>,
) -> RecordingMode {
    if input_mode == RecordingMode::Toggle {
        RecordingMode::Toggle
    } else {
        active_recording_mode.unwrap_or(input_mode)
    }
}

/// Serialises all transcription lifecycle events through a single thread
/// to eliminate race conditions between keyboard shortcuts, signals, and
/// the async transcribe-paste pipeline.
pub struct TranscriptionCoordinator {
    tx: Sender<Command>,
}

pub fn is_transcribe_binding(id: &str) -> bool {
    id == "transcribe" || id == "transcribe_with_post_process"
}

impl TranscriptionCoordinator {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut stage = Stage::Idle;
                let mut last_press: Option<Instant> = None;
                let mut pending_release: Option<PendingRelease> = None;
                let mut hybrid_press_started: Option<(String, Instant)> = None;
                let mut latched_binding: Option<String> = None;
                let mut consumed_release_binding: Option<String> = None;

                loop {
                    let cmd = if let Some(pending) = &pending_release {
                        match rx.recv_timeout(
                            pending.deadline.saturating_duration_since(Instant::now()),
                        ) {
                            Ok(cmd) => cmd,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some(pending) = pending_release.take() {
                                    if matches!(&stage, Stage::Recording { binding_id, .. } if binding_id == &pending.binding_id)
                                    {
                                        match pending.recording_mode {
                                            RecordingMode::PushToTalk => stop(
                                                &app,
                                                &mut stage,
                                                &pending.binding_id,
                                                &pending.hotkey_string,
                                            ),
                                            RecordingMode::TapOrHold => {
                                                if is_short_tap(
                                                    pending.press_started_at,
                                                    pending.released_at,
                                                ) {
                                                    debug!(
                                                        "Latched recording for '{}' after a short tap",
                                                        pending.binding_id
                                                    );
                                                    latched_binding = Some(pending.binding_id);
                                                } else {
                                                    stop(
                                                        &app,
                                                        &mut stage,
                                                        &pending.binding_id,
                                                        &pending.hotkey_string,
                                                    );
                                                }
                                            }
                                            RecordingMode::Toggle => {}
                                        }
                                    }
                                    hybrid_press_started = None;
                                }
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(cmd) => cmd,
                            Err(_) => break,
                        }
                    };

                    match cmd {
                        Command::Input {
                            binding_id,
                            hotkey_string,
                            is_pressed,
                            recording_mode,
                        } => {
                            if !is_pressed
                                && consumed_release_binding.as_deref() == Some(&binding_id)
                            {
                                consumed_release_binding = None;
                                continue;
                            }

                            let pending_release_binding = pending_release
                                .as_ref()
                                .map(|pending| pending.binding_id.as_str());
                            let recording_binding = match &stage {
                                Stage::Recording { binding_id, .. } => Some(binding_id.as_str()),
                                _ => None,
                            };
                            // CLI and signal inputs are explicitly submitted as
                            // Toggle mode. Preserve their legacy press-only
                            // behavior even if a keyboard-held recording was
                            // started under another mode.
                            let active_mode = effective_recording_mode(
                                recording_mode,
                                match &stage {
                                    Stage::Recording { recording_mode, .. } => {
                                        Some(*recording_mode)
                                    }
                                    _ => None,
                                },
                            );

                            match classify_release_event(
                                pending_release_binding,
                                is_pressed,
                                active_mode,
                                &binding_id,
                                recording_binding,
                            ) {
                                PttAction::CancelRelease => {
                                    pending_release = None;
                                    continue;
                                }
                                PttAction::DeferRelease => {
                                    let now = Instant::now();
                                    let press_started_at = hybrid_press_started
                                        .as_ref()
                                        .filter(|(id, _)| id == &binding_id)
                                        .map(|(_, started)| *started)
                                        .unwrap_or(now);
                                    pending_release = Some(PendingRelease {
                                        binding_id,
                                        hotkey_string,
                                        deadline: now + RELEASE_GRACE,
                                        recording_mode: active_mode,
                                        press_started_at,
                                        released_at: now,
                                    });
                                    continue;
                                }
                                PttAction::Passthrough => {}
                            }

                            // Debounce rapid-fire press events (key repeat / double-tap).
                            // Push-to-talk releases may be deferred above to absorb X11 auto-repeat.
                            if is_pressed {
                                let now = Instant::now();
                                if last_press.is_some_and(|t| now.duration_since(t) < DEBOUNCE) {
                                    debug!("Debounced press for '{binding_id}'");
                                    continue;
                                }
                                last_press = Some(now);
                            }

                            if active_mode == RecordingMode::PushToTalk {
                                if is_pressed && matches!(stage, Stage::Idle) {
                                    start(
                                        &app,
                                        &mut stage,
                                        &binding_id,
                                        &hotkey_string,
                                        active_mode,
                                    );
                                }
                            } else if active_mode == RecordingMode::TapOrHold && is_pressed {
                                match &stage {
                                    Stage::Idle => {
                                        let pressed_at = Instant::now();
                                        start(
                                            &app,
                                            &mut stage,
                                            &binding_id,
                                            &hotkey_string,
                                            active_mode,
                                        );
                                        if matches!(&stage, Stage::Recording { binding_id: id, .. } if id == &binding_id)
                                        {
                                            hybrid_press_started = Some((binding_id, pressed_at));
                                        } else {
                                            hybrid_press_started = None;
                                            latched_binding = None;
                                        }
                                    }
                                    Stage::Recording { binding_id: id, .. }
                                        if id == &binding_id
                                            && latched_binding.as_deref() == Some(&binding_id) =>
                                    {
                                        pending_release = None;
                                        hybrid_press_started = None;
                                        latched_binding = None;
                                        consumed_release_binding = Some(binding_id.clone());
                                        stop(&app, &mut stage, &binding_id, &hotkey_string);
                                    }
                                    _ => {
                                        debug!("Ignoring press for '{binding_id}': pipeline busy")
                                    }
                                }
                            } else if active_mode == RecordingMode::Toggle && is_pressed {
                                match &stage {
                                    Stage::Idle => start(
                                        &app,
                                        &mut stage,
                                        &binding_id,
                                        &hotkey_string,
                                        active_mode,
                                    ),
                                    Stage::Recording { binding_id: id, .. }
                                        if id == &binding_id =>
                                    {
                                        stop(&app, &mut stage, &binding_id, &hotkey_string);
                                    }
                                    _ => debug!("Ignoring press for '{binding_id}': pipeline busy"),
                                }
                            }
                        }
                        Command::Cancel {
                            recording_was_active,
                        } => {
                            pending_release = None;
                            hybrid_press_started = None;
                            latched_binding = None;
                            consumed_release_binding = None;
                            // Don't reset during processing — wait for the pipeline to finish.
                            if !matches!(stage, Stage::Processing)
                                && (recording_was_active
                                    || matches!(stage, Stage::Recording { .. }))
                            {
                                stage = Stage::Idle;
                            }
                        }
                        Command::ShortcutsSuspended => {
                            pending_release = None;
                            hybrid_press_started = None;
                            latched_binding = None;
                            consumed_release_binding = None;
                        }
                        Command::ProcessingFinished => {
                            pending_release = None;
                            hybrid_press_started = None;
                            latched_binding = None;
                            consumed_release_binding = None;
                            stage = Stage::Idle;
                        }
                    }
                }
                debug!("Transcription coordinator exited");
            }));
            if let Err(e) = result {
                error!("Transcription coordinator panicked: {e:?}");
            }
        });

        Self { tx }
    }

    /// Send a keyboard/signal input event for a transcribe binding.
    /// Signals and CLI use `RecordingMode::Toggle` with a press-only event.
    pub fn send_input(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        recording_mode: RecordingMode,
    ) {
        if self
            .tx
            .send(Command::Input {
                binding_id: binding_id.to_string(),
                hotkey_string: hotkey_string.to_string(),
                is_pressed,
                recording_mode,
            })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_cancel(&self, recording_was_active: bool) {
        if self
            .tx
            .send(Command::Cancel {
                recording_was_active,
            })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_processing_finished(&self) {
        if self.tx.send(Command::ProcessingFinished).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }

    /// Clear transient shortcut state while shortcuts are unregistered for
    /// recording a new binding. The active pipeline, if any, is left alone.
    pub fn notify_shortcuts_suspended(&self) {
        if self.tx.send(Command::ShortcutsSuspended).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }
}

fn start(
    app: &AppHandle,
    stage: &mut Stage,
    binding_id: &str,
    hotkey_string: &str,
    recording_mode: RecordingMode,
) {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return;
    };
    action.start(app, binding_id, hotkey_string);
    if app
        .try_state::<Arc<AudioRecordingManager>>()
        .is_some_and(|a| a.is_recording())
    {
        *stage = Stage::Recording {
            binding_id: binding_id.to_string(),
            recording_mode,
        };
    } else {
        debug!("Start for '{binding_id}' did not begin recording; staying idle");
    }
}

fn stop(app: &AppHandle, stage: &mut Stage, binding_id: &str, hotkey_string: &str) {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return;
    };
    action.stop(app, binding_id, hotkey_string);
    *stage = Stage::Processing;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_to_talk_release_while_recording_defers_release() {
        assert_eq!(
            classify_release_event(
                None,
                false,
                RecordingMode::PushToTalk,
                "transcribe",
                Some("transcribe"),
            ),
            PttAction::DeferRelease
        );
    }

    #[test]
    fn push_to_talk_press_matching_pending_release_cancels_release() {
        assert_eq!(
            classify_release_event(
                Some("transcribe"),
                true,
                RecordingMode::PushToTalk,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::CancelRelease
        );
    }

    #[test]
    fn toggle_mode_press_and_release_pass_through() {
        assert_eq!(
            classify_release_event(
                Some("transcribe"),
                true,
                RecordingMode::Toggle,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
        assert_eq!(
            classify_release_event(
                None,
                false,
                RecordingMode::Toggle,
                "transcribe",
                Some("transcribe"),
            ),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_for_different_binding_than_pending_release_passes_through() {
        assert_eq!(
            classify_release_event(
                Some("transcribe"),
                true,
                RecordingMode::PushToTalk,
                "transcribe_with_post_process",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_matching_pending_release_cancels_without_recording_state() {
        assert_eq!(
            classify_release_event(
                Some("transcribe"),
                true,
                RecordingMode::PushToTalk,
                "transcribe",
                None,
            ),
            PttAction::CancelRelease
        );
    }

    // ---------------------------------------------------------------------
    // Sequence-level regression coverage for issue #1539.
    //
    // Under X11 key auto-repeat, holding a push-to-talk key does not emit one
    // long press. It emits the initial press followed by a stream of
    // synthesized release/press pairs, then a single genuine release on key-up.
    // Before the fix, every synthesized release passed straight through and
    // stopped recording, so holding the key "rapidly toggled" recording on and
    // off. The fix defers each release for a short grace window and cancels it
    // when the matching auto-repeat press arrives.
    //
    // The unit tests above assert `classify_release_event` in isolation. The
    // simulator below threads that classifier through the same `pending_release`
    // / `stage` state transitions the coordinator loop performs (lines that
    // handle `Command::Input` and the `recv_timeout` grace expiry), so a whole
    // event burst can be exercised deterministically without a Tauri AppHandle
    // or real timers.
    // ---------------------------------------------------------------------

    const BINDING: &str = "transcribe";

    #[derive(Clone, Copy)]
    enum Ev {
        /// A key-down event (real initial press or a synthesized auto-repeat press).
        Press,
        /// A key-up event (synthesized auto-repeat release or the genuine key-up).
        Release,
        /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving.
        Grace,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum SimStage {
        Idle,
        Recording,
        Processing,
    }

    struct SimResult {
        starts: u32,
        stops: u32,
        stage: SimStage,
    }

    /// Mirror of the coordinator loop's decision logic for a single push-to-talk
    /// binding: it calls the real `classify_release_event` and applies the exact same
    /// Defer / Cancel / debounce / start / stop transitions.
    fn simulate(events: &[Ev]) -> SimResult {
        let mut stage = SimStage::Idle;
        let mut pending: Option<String> = None;
        let mut last_press_ms: Option<u64> = None;
        let mut clock_ms: u64 = 0;
        let mut starts = 0u32;
        let mut stops = 0u32;
        let debounce_ms = DEBOUNCE.as_millis() as u64;

        for ev in events {
            // Auto-repeat events arrive a few ms apart, well inside DEBOUNCE.
            clock_ms += 5;

            match ev {
                Ev::Grace => {
                    // Coordinator's `RecvTimeoutError::Timeout` arm: fire the
                    // deferred release iff we are still recording that binding.
                    if let Some(pending_binding) = pending.take() {
                        if stage == SimStage::Recording && pending_binding == BINDING {
                            stage = SimStage::Processing;
                            stops += 1;
                        }
                    }
                }
                Ev::Press | Ev::Release => {
                    let is_pressed = matches!(ev, Ev::Press);
                    let pending_binding = pending.as_deref();
                    let recording_binding = if stage == SimStage::Recording {
                        Some(BINDING)
                    } else {
                        None
                    };

                    match classify_release_event(
                        pending_binding,
                        is_pressed,
                        RecordingMode::PushToTalk,
                        BINDING,
                        recording_binding,
                    ) {
                        PttAction::CancelRelease => {
                            pending = None;
                            continue;
                        }
                        PttAction::DeferRelease => {
                            pending = Some(BINDING.to_string());
                            continue;
                        }
                        PttAction::Passthrough => {}
                    }

                    if is_pressed {
                        if last_press_ms.is_some_and(|t| clock_ms - t < debounce_ms) {
                            continue;
                        }
                        last_press_ms = Some(clock_ms);
                    }

                    if is_pressed && stage == SimStage::Idle {
                        stage = SimStage::Recording;
                        starts += 1;
                    } else if !is_pressed && stage == SimStage::Recording {
                        stage = SimStage::Processing;
                        stops += 1;
                    }
                }
            }
        }

        SimResult {
            starts,
            stops,
            stage,
        }
    }

    /// Initial press plus several synthesized release/press pairs, as X11 emits
    /// while a push-to-talk key is held down.
    fn autorepeat_burst() -> Vec<Ev> {
        let mut events = vec![Ev::Press];
        for _ in 0..6 {
            events.push(Ev::Release);
            events.push(Ev::Press);
        }
        events
    }

    /// Regression for #1539: a burst of X11 auto-repeat release/press pairs must
    /// not stop recording. Before the fix the first synthesized release stopped
    /// recording immediately (stops == 1, stage left Recording), which produced
    /// the rapid on/off toggling. With the fix the releases are coalesced and
    /// recording stays continuously active for the whole burst.
    #[test]
    fn x11_autorepeat_burst_does_not_toggle_recording() {
        let result = simulate(&autorepeat_burst());
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 0,
            "synthesized auto-repeat releases must not stop recording mid-burst"
        );
        assert_eq!(
            result.stage,
            SimStage::Recording,
            "recording must remain active across the entire auto-repeat burst"
        );
    }

    /// Complements the burst test: once the key is genuinely released and the
    /// grace window elapses with no re-press, recording stops exactly once. This
    /// proves the debounce only coalesces synthesized releases and does not wedge
    /// the coordinator or swallow the real key-up.
    #[test]
    fn genuine_release_after_grace_stops_recording_once() {
        let mut events = autorepeat_burst();
        events.push(Ev::Release); // genuine key-up
        events.push(Ev::Grace); // grace window elapses, no cancelling press
        let result = simulate(&events);
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 1,
            "a genuine release should stop recording exactly once"
        );
        assert_eq!(result.stage, SimStage::Processing);
    }

    #[derive(Debug, PartialEq, Eq)]
    struct HybridResult {
        starts: u32,
        stops: u32,
        stage: SimStage,
        latched: bool,
        consumed_release: bool,
    }

    /// Deterministic mirror of the Tap-or-hold coordinator transitions. Times
    /// are event timestamps, so the 250 ms boundary is independent of the
    /// release-grace timer used to absorb X11 auto-repeat events.
    struct HybridSimulation {
        stage: SimStage,
        press_started_at: Option<u64>,
        pending_release: Option<(u64, u64)>,
        latched: bool,
        consumed_release: bool,
        starts: u32,
        stops: u32,
    }

    impl HybridSimulation {
        fn new() -> Self {
            Self {
                stage: SimStage::Idle,
                press_started_at: None,
                pending_release: None,
                latched: false,
                consumed_release: false,
                starts: 0,
                stops: 0,
            }
        }

        fn press(&mut self, at_ms: u64) {
            if self.pending_release.take().is_some() {
                return; // synthesized X11 repeat press
            }
            match &self.stage {
                SimStage::Idle => {
                    self.stage = SimStage::Recording;
                    self.press_started_at = Some(at_ms);
                    self.starts += 1;
                }
                SimStage::Recording if self.latched => {
                    self.stage = SimStage::Processing;
                    self.press_started_at = None;
                    self.latched = false;
                    self.consumed_release = true;
                    self.stops += 1;
                }
                SimStage::Recording | SimStage::Processing => {}
            }
        }

        fn release(&mut self, at_ms: u64) {
            if self.consumed_release {
                self.consumed_release = false;
                return;
            }
            if self.stage == SimStage::Recording && self.pending_release.is_none() {
                self.pending_release = Some((self.press_started_at.unwrap(), at_ms));
            }
        }

        fn grace_elapsed(&mut self) {
            let Some((pressed_at, released_at)) = self.pending_release.take() else {
                return;
            };
            if self.stage != SimStage::Recording {
                return;
            }
            if released_at - pressed_at < TAP_OR_HOLD_THRESHOLD.as_millis() as u64 {
                self.press_started_at = None;
                self.latched = true;
            } else {
                self.stage = SimStage::Processing;
                self.press_started_at = None;
                self.stops += 1;
            }
        }

        fn cancel(&mut self) {
            self.stage = SimStage::Idle;
            self.press_started_at = None;
            self.pending_release = None;
            self.latched = false;
            self.consumed_release = false;
        }

        fn result(&self) -> HybridResult {
            HybridResult {
                starts: self.starts,
                stops: self.stops,
                stage: self.stage.clone(),
                latched: self.latched,
                consumed_release: self.consumed_release,
            }
        }
    }

    #[test]
    fn short_tap_latches_then_second_press_stops_and_consumes_release() {
        let mut sim = HybridSimulation::new();
        sim.press(0);
        sim.release(249);
        sim.grace_elapsed();
        assert_eq!(sim.result().stage, SimStage::Recording);
        assert!(sim.result().latched);

        sim.press(400);
        assert_eq!(sim.result().stage, SimStage::Processing);
        assert!(sim.result().consumed_release);
        sim.release(405);

        assert_eq!(
            sim.result(),
            HybridResult {
                starts: 1,
                stops: 1,
                stage: SimStage::Processing,
                latched: false,
                consumed_release: false,
            }
        );
    }

    #[test]
    fn exact_threshold_release_is_a_held_recording() {
        let mut sim = HybridSimulation::new();
        sim.press(0);
        sim.release(TAP_OR_HOLD_THRESHOLD.as_millis() as u64);
        sim.grace_elapsed();

        assert_eq!(sim.result().stage, SimStage::Processing);
        assert_eq!(sim.result().stops, 1);
        assert!(!sim.result().latched);
    }

    #[test]
    fn autorepeat_release_press_pair_preserves_original_hybrid_press_time() {
        let mut sim = HybridSimulation::new();
        sim.press(0);
        sim.release(100);
        sim.press(105); // synthesized repeat press cancels the pending release
        sim.release(600);
        sim.grace_elapsed();

        assert_eq!(sim.result().starts, 1);
        assert_eq!(sim.result().stops, 1);
        assert_eq!(sim.result().stage, SimStage::Processing);
    }

    #[test]
    fn cancel_clears_held_and_latched_hybrid_state() {
        let mut held = HybridSimulation::new();
        held.press(0);
        held.cancel();
        assert_eq!(held.result().stage, SimStage::Idle);
        assert!(!held.result().latched);

        let mut latched = HybridSimulation::new();
        latched.press(0);
        latched.release(10);
        latched.grace_elapsed();
        latched.cancel();
        assert_eq!(latched.result().stage, SimStage::Idle);
        assert!(!latched.result().latched);
    }

    #[test]
    fn threshold_helper_and_press_only_toggle_behavior_are_explicit() {
        let start = Instant::now();
        assert!(is_short_tap(
            start,
            start + TAP_OR_HOLD_THRESHOLD - Duration::from_millis(1)
        ));
        assert!(!is_short_tap(start, start + TAP_OR_HOLD_THRESHOLD));
        assert_eq!(
            classify_release_event(
                None,
                true,
                RecordingMode::Toggle,
                "transcribe",
                Some("transcribe"),
            ),
            PttAction::Passthrough,
            "a CLI/signal press is a regular toggle press regardless of UI mode"
        );
        assert_eq!(
            effective_recording_mode(RecordingMode::Toggle, Some(RecordingMode::PushToTalk),),
            RecordingMode::Toggle,
            "a CLI/signal press can stop an existing held recording"
        );
    }
}
