use std::collections::HashSet;
use std::iter::once;

use tokio::sync::oneshot;

use crate::executor::Barrier;

#[derive(Debug)]
pub(super) enum ManagedBarrierState {
    /// Currently no barrier on the flight.
    Pending {
        /// Last epoch of barriers.
        // TODO: an initial value should be specified
        last_epoch: Option<u64>,
    },

    /// Barriers from some actors have been collected and stashed, however no `send_barrier`
    /// request from the meta service is issued.
    Stashed {
        epoch: u64,

        /// Actor ids we've collected and stashed.
        collected_actors: HashSet<u32>,
    },

    /// Meta service has issued a `send_barrier` request. We're collecting barriers now.
    Issued {
        epoch: u64,

        /// Actor ids remaining to be collected.
        remaining_actors: HashSet<u32>,

        /// Notify that the collection is finished.
        collect_notifier: oneshot::Sender<()>,
    },
}

impl ManagedBarrierState {
    /// Notify if we have collected barriers from all actor ids. The state must be `Issued`.
    fn may_notify(&mut self) {
        let (epoch, to_notify) = match self {
            ManagedBarrierState::Issued {
                epoch,
                remaining_actors,
                ..
            } => (*epoch, remaining_actors.is_empty()),

            _ => unreachable!(),
        };

        if to_notify {
            let state = std::mem::replace(
                self,
                ManagedBarrierState::Pending {
                    last_epoch: Some(epoch),
                },
            );

            match state {
                ManagedBarrierState::Issued {
                    collect_notifier, ..
                } => {
                    // Notify about barrier finishing.
                    if collect_notifier.send(()).is_err() {
                        warn!("failed to notify barrier collection with epoch {}", epoch)
                    }
                }

                _ => unreachable!(),
            }
        }
    }

    /// Collect a `barrier` from the actor with `actor_id`.
    pub(super) fn collect(&mut self, actor_id: u32, barrier: &Barrier) {
        tracing::trace!(
            target: "events::stream::barrier::collect_barrier",
            "collect_barrier: epoch = {}, actor_id = {}, state = {:#?}",
            barrier.epoch.curr,
            actor_id,
            self
        );

        match self {
            ManagedBarrierState::Pending { last_epoch } => {
                if let Some(last_epoch) = *last_epoch {
                    assert_eq!(barrier.epoch.prev, last_epoch)
                }

                *self = Self::Stashed {
                    epoch: barrier.epoch.curr,
                    collected_actors: once(actor_id).collect(),
                }
            }

            ManagedBarrierState::Stashed {
                epoch,
                collected_actors,
            } => {
                assert_eq!(barrier.epoch.curr, *epoch);

                let new = collected_actors.insert(actor_id);
                assert!(new);
            }

            ManagedBarrierState::Issued {
                epoch,
                remaining_actors,
                ..
            } => {
                assert_eq!(barrier.epoch.curr, *epoch);

                let exist = remaining_actors.remove(&actor_id);
                assert!(exist);
                self.may_notify();
            }
        }
    }

    /// When the meta service issues a `send_barrier` request, call this function to transform to
    /// `Issued` and start to collect or to notify.
    pub(super) fn transform_to_issued(
        &mut self,
        barrier: &Barrier,
        actor_ids_to_collect: impl IntoIterator<Item = u32>,
        collect_notifier: oneshot::Sender<()>,
    ) {
        match self {
            ManagedBarrierState::Pending { .. } => {
                let remaining_actors = actor_ids_to_collect.into_iter().collect();

                *self = Self::Issued {
                    epoch: barrier.epoch.curr,
                    remaining_actors,
                    collect_notifier,
                };
                self.may_notify();
            }

            ManagedBarrierState::Stashed {
                epoch,
                collected_actors,
            } => {
                assert_eq!(barrier.epoch.curr, *epoch);

                let remaining_actors = actor_ids_to_collect
                    .into_iter()
                    .filter(|a| !collected_actors.contains(a))
                    .collect();

                *self = Self::Issued {
                    epoch: barrier.epoch.curr,
                    remaining_actors,
                    collect_notifier,
                };
                self.may_notify();
            }

            ManagedBarrierState::Issued { .. } => panic!("barrier state has already been `Issued`"),
        }
    }
}