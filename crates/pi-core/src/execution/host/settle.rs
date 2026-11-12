//! Return executions to their owning conversation before rendering their outcome.

use crate::core::Core;
use crate::driver::Ended;
use crate::execution::{Completion, Done, Kind, Request, RunError, RunResult};

pub enum Settlement {
    Ignored,
    Removed(Option<Completion>),
    Returned(Returned),
}

pub struct Returned {
    pub lane: usize,
    pub request: Request,
    pub kind: Kind,
    pub result: RunResult,
    pub ended: Ended,
    pub unsend: bool,
    pub returned: bool,
    pub recovery: Recovery,
}

pub enum Recovery {
    Unneeded,
    Restored,
    Failed(String),
}

impl Returned {
    pub fn completion(&self) -> Option<Completion> {
        super::super::operation_completion(self.request, &self.kind, Some(&self.result))
    }
}

impl Core {
    /// Restore ownership and persist state without consulting the selected session.
    pub fn settle_execution(&mut self, done: Done) -> Settlement {
        let Some(at) = self.position_of(done.request.lane) else {
            return Settlement::Removed(done.completion());
        };
        if !self.lanes[at].accepts_completion(done.request) {
            tracing::warn!(target:"pi::session",request=done.request.id,lane=done.request.lane,"ignored stale execution completion");
            return Settlement::Ignored;
        }
        let unsend = self.lanes.at_mut(at).finish();
        let ended = if unsend { Ended::Unsent } else { done.ended() };
        let Done { request, kind, ran } = done;
        let returned = ran.is_some();
        let (result, recovery) = match ran {
            Some((session, result)) => {
                self.lanes.at_mut(at).return_session(session);
                (result, Recovery::Unneeded)
            }
            None => {
                let lane = &self.lanes[at];
                let path = self.store.path_of(lane.root(), lane.id());
                let recovery = match pi_store::session::Store::read(&path) {
                    Ok(stored) => {
                        self.lanes.at_mut(at).return_session(stored.into_session());
                        tracing::info!(target:"pi::session",request=request.id,lane=request.lane,"execution recovered from archive");
                        Recovery::Restored
                    }
                    Err(error) => {
                        let why = format!("{error:#}");
                        tracing::warn!(target:"pi::session",request=request.id,lane=request.lane,%why,"execution archive unavailable");
                        Recovery::Failed(why)
                    }
                };
                (Err(RunError::Crashed), recovery)
            }
        };
        // A compaction's summary call is the session's spend, not the number
        // of the turn that ran last: counted past, so the row keeps saying
        // what that turn cost.
        if matches!(kind, Kind::Compact(_)) {
            self.lanes.at_mut(at).pass_by_spend();
        }
        let persist = match kind {
            Kind::Turn | Kind::Bash { .. } | Kind::BashBackground { .. } => true,
            Kind::Compact(report) => report.is_some(),
        };
        if persist && !matches!(recovery, Recovery::Failed(_)) {
            self.save_lane_behind(at);
        }
        match &ended {
            Ended::Done => {
                tracing::info!(target:"pi::session",request=request.id,lane=request.lane,"execution ended")
            }
            Ended::Stopped => {
                tracing::info!(target:"pi::session",request=request.id,lane=request.lane,"execution stopped")
            }
            Ended::Unsent => {
                tracing::info!(target:"pi::session",request=request.id,lane=request.lane,"execution unsent")
            }
            Ended::Failed(why) => {
                tracing::warn!(target:"pi::session",request=request.id,lane=request.lane,%why,"execution failed")
            }
        }
        Settlement::Returned(Returned {
            lane: at,
            request,
            kind,
            result,
            ended,
            unsend,
            returned,
            recovery,
        })
    }
}
