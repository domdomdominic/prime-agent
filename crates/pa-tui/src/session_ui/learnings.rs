//! The learnings dock group and the review view: the cached pending
//! learnings, their background refresh, and the keep/revert requests.
use super::{
    key_event_to_id, mpsc, AgentView, DaemonCommand, Duration, KeyEvent, Map, Result, SessionUi,
    UI_REQUEST_TIMEOUT_MS,
};
use crate::learnings_review::{
    parse_pending_learnings, LearningsReview, LearningsReviewAction, PendingLearning, Verdict,
};

/// A landed `get_learnings` refresh for the session it was issued under.
pub(crate) struct LearningsUpdate {
    active_session_id: String,
    learnings: Result<Vec<PendingLearning>, String>,
}

impl SessionUi {
    /// Refetch the pending learnings in the background; the dock and an
    /// open review fold the result in [`Self::apply_learnings_update`].
    pub(crate) fn spawn_learnings_refresh(&self) {
        let updates: mpsc::UnboundedSender<LearningsUpdate> = self.learnings_updates.clone();
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let request = DaemonCommand::GetLearnings {
                id: None,
                active_session_id: active_session_id.clone(),
                rest: Map::default(),
            };
            let learnings = match tokio::time::timeout(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                client.request_ok(request),
            )
            .await
            {
                Ok(Ok(data)) => Ok(parse_pending_learnings(&data)),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(_) => Err("timed out waiting for the Prime Agent daemon response".to_string()),
            };
            let _ = updates.send(LearningsUpdate {
                active_session_id,
                learnings,
            });
        });
    }

    /// Fold a landed refresh: a reply for another session is dropped, and a
    /// failed one keeps the last list.
    pub(crate) fn apply_learnings_update(&mut self, update: LearningsUpdate, view: &mut AgentView) {
        if update.active_session_id != self.active_session_id {
            return;
        }
        if let Ok(learnings) = update.learnings {
            self.pending_learnings = learnings;
            self.sync_activity_dock(view);
        }
    }

    /// Open the review view over the cached pending learnings; the keypress
    /// never waits on the daemon.
    pub(crate) fn open_learnings_review(&mut self, view: &mut AgentView) {
        self.subagents_focused = false;
        // The close hands the focus back to the dock's own group.
        self.activity_group = crate::chrome::ActivityGroup::Learnings;
        self.spawn_learnings_refresh();
        // The card and its rationale need more rows than a picker list.
        let rows = usize::from(view.terminal_rows()) * 2 / 3;
        view.learnings_review = Some(LearningsReview::new(
            self.pending_learnings.clone(),
            rows.max(super::picker_viewport_rows(view.terminal_rows())),
        ));
        self.dirty = true;
    }

    /// One key press while the review view is open.
    pub(crate) async fn handle_learnings_review_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let action = view
            .learnings_review
            .as_mut()
            .map(|review| review.handle_key(&id, view.editor.keybindings()));
        match action {
            Some(LearningsReviewAction::Close) => {
                view.learnings_review = None;
                // The exit restores the dock's own group (the operator's 2026-09-26
                // panel-exit ruling).
                self.focus_activity_dock(view);
            }
            Some(LearningsReviewAction::Review {
                refinement_id,
                verdict,
            }) => self.send_review(refinement_id, verdict, view).await,
            Some(LearningsReviewAction::None) | None => {}
        }
        self.dirty = true;
        Ok(())
    }

    /// Record one verdict. A revert's rollback card reaches the transcript
    /// through the session's own events; the view drops the learning and
    /// closes after the last one.
    async fn send_review(&mut self, refinement_id: String, verdict: Verdict, view: &mut AgentView) {
        let (verdict_name, failed_criteria, note) = match &verdict {
            Verdict::Keep => ("keep", Vec::new(), None),
            Verdict::Revert {
                failed_criteria,
                note,
            } => (
                "revert",
                failed_criteria.iter().map(ToString::to_string).collect(),
                note.clone(),
            ),
        };
        let request = DaemonCommand::ReviewLearning {
            id: None,
            active_session_id: self.active_session_id.clone(),
            refinement_id: refinement_id.clone(),
            verdict: verdict_name.to_string(),
            failed_criteria,
            note,
            rest: Map::default(),
        };
        let reviewed = self
            .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), request)
            .await;
        let Some(review) = view.learnings_review.as_mut() else {
            return;
        };
        match reviewed {
            Ok(_) => {
                review.reviewed(&refinement_id);
                self.pending_learnings
                    .retain(|learning| learning.id != refinement_id);
                if review.is_empty() {
                    view.learnings_review = None;
                }
                if verdict == Verdict::Keep {
                    self.toast(&format!("Kept {refinement_id}"), view);
                }
            }
            Err(error) => review.set_error(format!("{error:#}")),
        }
        self.sync_activity_dock(view);
    }
}
