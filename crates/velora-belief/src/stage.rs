use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use velora_event::Sequence;
use velora_log::{debug, info, warn};
use velora_loop::{Event, Stage, StageContext, StageError};

use crate::belief::Belief;
use crate::claim::ClaimKey;
use crate::policy::{ConfidenceFirst, RevisionPolicy};
use crate::store::{BeliefError, BeliefStore, StoreStats};

/// A shared, cloneable view of a live belief store.
///
/// The loop owns its stages, so the only way to ask what is believed while the
/// process is running is to hold a handle to the same store the stage folds
/// into. Cloning a handle is cheap; the store behind it is one lock.
#[derive(Clone, Debug)]
pub struct BeliefHandle {
    inner: Arc<RwLock<BeliefStore>>,
}

impl Default for BeliefHandle {
    fn default() -> Self {
        BeliefHandle::new()
    }
}

impl BeliefHandle {
    /// Creates an empty store using the default revision policy.
    pub fn new() -> Self {
        BeliefHandle::with_policy(Arc::new(ConfidenceFirst))
    }

    /// Creates an empty store with a caller-supplied policy.
    pub fn with_policy(policy: Arc<dyn RevisionPolicy>) -> Self {
        BeliefHandle {
            inner: Arc::new(RwLock::new(BeliefStore::with_policy(policy))),
        }
    }

    /// The belief currently held about `key`.
    pub fn get(&self, key: &ClaimKey) -> Option<Belief> {
        self.read().get(key).cloned()
    }

    /// How many beliefs are held.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// What the store has taken in, refused, and currently holds.
    pub fn stats(&self) -> StoreStats {
        self.read().stats()
    }

    /// Whether no beliefs are held.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Every held belief, ordered by key.
    pub fn beliefs(&self) -> Vec<Belief> {
        self.read().iter().cloned().collect()
    }

    /// The first `limit` beliefs, ordered by key.
    ///
    /// Bounded so that a caller assembling a prompt cannot be handed an
    /// unbounded amount of state to render.
    pub fn beliefs_up_to(&self, limit: usize) -> Vec<Belief> {
        self.read().iter().take(limit).cloned().collect()
    }

    /// The beliefs most likely to be affected by some text, capped.
    ///
    /// See [`velora_belief::BeliefStore::relevant_to`] for how the ranking works.
    pub fn beliefs_relevant_to(&self, text: &str, limit: usize) -> Vec<Belief> {
        self.read().relevant_to(text, limit)
    }

    /// Whether anything disagrees with the held belief for `key`.
    pub fn is_contested(&self, key: &ClaimKey) -> bool {
        self.read().is_contested(key)
    }

    /// Claims that disagreed with the held belief for `key`.
    pub fn challenges(&self, key: &ClaimKey) -> Vec<Belief> {
        self.read().challenges(key).to_vec()
    }

    /// A consistent copy of the whole store.
    pub fn snapshot(&self) -> BeliefStore {
        self.read().clone()
    }

    /// Reads the store, surviving a panic elsewhere rather than cascading.
    fn read(&self) -> RwLockReadGuard<'_, BeliefStore> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the store, surviving a panic elsewhere rather than cascading.
    ///
    /// A belief store that refuses every future update because an unrelated
    /// task panicked once would be a worse failure than the panic.
    fn write(&self) -> RwLockWriteGuard<'_, BeliefStore> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Folds belief events into a [`BeliefStore`].
///
/// This is the projection. It is the only thing that decides what is currently
/// believed, it holds no state that is not derived from events, and it rebuilds
/// itself from history on start-up — so the belief store can always be thrown
/// away and recomputed.
///
/// # What it refuses
///
/// A malformed event, or an assertion whose evidence does not already exist, is
/// refused with a warning and the stage carries on. That is not error handling:
/// it is the belief system declining to hold something it cannot justify. One
/// bad assertion must not stop the process, and it must not be believed either.
pub struct BeliefStage {
    handle: BeliefHandle,
}

impl std::fmt::Debug for BeliefStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BeliefStage")
            .field("store", &self.handle.stats())
            .finish()
    }
}

impl BeliefStage {
    /// Folds into the store behind `handle`.
    pub fn new(handle: BeliefHandle) -> Self {
        BeliefStage { handle }
    }
}

#[async_trait]
impl Stage for BeliefStage {
    fn name(&self) -> &'static str {
        "beliefs"
    }

    /// A belief store is derived state, so it is rebuilt, never resumed.
    fn rebuild_from_history(&self) -> bool {
        true
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        let mut store = self.handle.write();

        match store.apply_payload(event.kind(), event.version(), event.payload(), sequence) {
            Ok(None) => Ok(()),
            Ok(Some(outcome)) => {
                if ctx.is_replaying() {
                    debug!(
                        sequence = sequence.get(),
                        kind = %event.kind(),
                        outcome = %outcome,
                        "belief rebuilt from history"
                    );
                } else {
                    info!(
                        sequence = sequence.get(),
                        kind = %event.kind(),
                        outcome = %outcome,
                        held = store.len(),
                        "belief updated"
                    );
                }
                Ok(())
            }
            Err(err) => {
                let reason = match &err {
                    BeliefError::Decode(_) => "malformed",
                    BeliefError::Rejected(_) => "unjustified",
                };
                warn!(
                    sequence = sequence.get(),
                    kind = %event.kind(),
                    reason,
                    detail = %err,
                    "belief event refused; nothing is held from it"
                );
                Ok(())
            }
        }
    }
}
