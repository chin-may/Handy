//! In-memory ownership of the most recent AI cleanup result.
//!
//! History retains every cleanup result, but the insert hotkey must only ever use
//! the current dictation's result. A generation makes late completions harmless.

use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LatestCleanupState {
    Empty,
    Pending {
        generation: u64,
        insert_when_ready: bool,
    },
    Ready {
        generation: u64,
        text: String,
    },
    Failed {
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertLatestOutcome {
    Insert(String),
    Pending,
    Unavailable,
    Empty,
}

/// Completion is only observable for the current dictation. A late cleanup can
/// still update its History row, but must not update the overlay or insertion
/// target for a newer dictation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestCleanupCompletion {
    Ready,
    Insert(String),
}

/// Coordinates one explicit insertion operation for the latest cleanup result.
pub struct LatestCleanupCoordinator {
    state: Mutex<LatestCleanupState>,
    next_generation: Mutex<u64>,
}

impl LatestCleanupCoordinator {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(LatestCleanupState::Empty),
            next_generation: Mutex::new(0),
        }
    }

    /// Invalidates the current target as soon as a new recording begins.
    pub fn invalidate(&self) {
        *self.state.lock().expect("latest cleanup state poisoned") = LatestCleanupState::Empty;
    }

    pub fn begin(&self) -> u64 {
        let mut next = self
            .next_generation
            .lock()
            .expect("latest cleanup generation poisoned");
        *next += 1;
        let generation = *next;
        *self.state.lock().expect("latest cleanup state poisoned") = LatestCleanupState::Pending {
            generation,
            insert_when_ready: false,
        };
        generation
    }

    /// Returns a completion only when it belongs to the current dictation.
    pub fn complete(&self, generation: u64, text: String) -> Option<LatestCleanupCompletion> {
        let mut state = self.state.lock().expect("latest cleanup state poisoned");
        let LatestCleanupState::Pending {
            generation: pending_generation,
            insert_when_ready,
        } = &*state
        else {
            return None;
        };
        if *pending_generation != generation {
            return None;
        }
        let completion = if *insert_when_ready {
            LatestCleanupCompletion::Insert(text.clone())
        } else {
            LatestCleanupCompletion::Ready
        };
        *state = LatestCleanupState::Ready { generation, text };
        Some(completion)
    }

    pub fn fail(&self, generation: u64) {
        let mut state = self.state.lock().expect("latest cleanup state poisoned");
        if matches!(&*state, LatestCleanupState::Pending { generation: current, .. } if *current == generation)
        {
            *state = LatestCleanupState::Failed { generation };
        }
    }

    pub fn request_insert(&self) -> InsertLatestOutcome {
        let mut state = self.state.lock().expect("latest cleanup state poisoned");
        match &mut *state {
            LatestCleanupState::Empty => InsertLatestOutcome::Empty,
            LatestCleanupState::Failed { .. } => InsertLatestOutcome::Unavailable,
            LatestCleanupState::Ready { text, .. } => InsertLatestOutcome::Insert(text.clone()),
            LatestCleanupState::Pending {
                insert_when_ready, ..
            } => {
                *insert_when_ready = true;
                InsertLatestOutcome::Pending
            }
        }
    }
}

impl Default for LatestCleanupCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{InsertLatestOutcome, LatestCleanupCompletion, LatestCleanupCoordinator};

    #[test]
    fn ready_result_is_inserted() {
        let coordinator = LatestCleanupCoordinator::new();
        let generation = coordinator.begin();
        assert_eq!(
            coordinator.complete(generation, "Cleaned".into()),
            Some(LatestCleanupCompletion::Ready)
        );
        assert_eq!(
            coordinator.request_insert(),
            InsertLatestOutcome::Insert("Cleaned".into())
        );
    }

    #[test]
    fn pending_insert_is_queued_once_and_completed_once() {
        let coordinator = LatestCleanupCoordinator::new();
        let generation = coordinator.begin();
        assert_eq!(coordinator.request_insert(), InsertLatestOutcome::Pending);
        assert_eq!(coordinator.request_insert(), InsertLatestOutcome::Pending);
        assert_eq!(
            coordinator.complete(generation, "Cleaned".into()),
            Some(LatestCleanupCompletion::Insert("Cleaned".into()))
        );
        assert_eq!(
            coordinator.request_insert(),
            InsertLatestOutcome::Insert("Cleaned".into())
        );
    }

    #[test]
    fn failure_and_empty_never_supply_text() {
        let coordinator = LatestCleanupCoordinator::new();
        assert_eq!(coordinator.request_insert(), InsertLatestOutcome::Empty);
        let generation = coordinator.begin();
        coordinator.fail(generation);
        assert_eq!(
            coordinator.request_insert(),
            InsertLatestOutcome::Unavailable
        );
    }

    #[test]
    fn newer_dictation_rejects_a_late_completion() {
        let coordinator = LatestCleanupCoordinator::new();
        let first = coordinator.begin();
        coordinator.invalidate();
        let second = coordinator.begin();
        assert_eq!(coordinator.complete(first, "Old".into()), None);
        assert_eq!(
            coordinator.complete(second, "New".into()),
            Some(LatestCleanupCompletion::Ready)
        );
        assert_eq!(
            coordinator.request_insert(),
            InsertLatestOutcome::Insert("New".into())
        );
    }
}
