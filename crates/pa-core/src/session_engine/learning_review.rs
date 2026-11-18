//! Learning review on a live session: list each learning with its review
//! state, and record a keep or revert verdict.

use pa_types::ai::UserContent;
use pa_types::session::CustomMessage;

use super::engine::SessionEngine;
use super::refine::{
    create_refinement_outcome_message, local_store_dir, run_refinement, CommittedRefinement,
    RefineOptions, RefinementSource,
};
use super::AgentSession;
use crate::refinement::executor::RefinerFn;
use crate::refinement::review::{
    append_review, changed_entries, is_learning, review_state, FailedCriteria, ReviewRecord,
    ReviewState, Verdict,
};
use crate::refinement::{
    infer_refinement_result_scope, load_harness_state, merge_refinement_history, HarnessScope,
    RefinementResult,
};

pub enum ReviewDecision {
    Keep,
    Revert {
        failed_criteria: FailedCriteria,
        note: Option<String>,
    },
}

/// A learning listed for review: the refinement-outcome card plus its review state.
pub const LEARNING_REVIEW_CUSTOM_TYPE: &str = "learning_review";

/// A recorded verdict, the learning's scope, plus the rollback a revert ran.
#[derive(Debug)]
pub struct ReviewOutcome {
    pub record: ReviewRecord,
    pub scope: HarnessScope,
    pub rollback: Option<CommittedRefinement>,
}

pub struct Learning {
    pub result: RefinementResult,
    pub scope: HarnessScope,
    pub state: ReviewState,
}

/// A rollback restores snapshots; planning it never reaches the model.
fn rollback_refiner() -> RefinerFn {
    Box::new(|_model, _system, _prompt| {
        Box::pin(async { anyhow::bail!("a rollback never calls the refiner model") })
    })
}

impl AgentSession {
    /// Every learning in both stores' history files and the session
    /// history, oldest first. A session without a local store has no local learnings.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has no refinement stores.
    pub async fn learnings(&self) -> anyhow::Result<Vec<Learning>> {
        let stores = self.refinement_stores()?;
        let session_history = self.session.lock().await.refinement_history();
        let history = merge_refinement_history(&stores.file_history(), &session_history);
        let reviews = stores.review_log();
        let global = load_harness_state(&stores.global_dir, HarnessScope::Global);
        let local = stores
            .local_dir
            .as_deref()
            .map(|dir| load_harness_state(dir, HarnessScope::Local));
        Ok(history
            .into_iter()
            .filter(is_learning)
            .filter_map(|result| {
                let scope = infer_refinement_result_scope(&result).unwrap_or(HarnessScope::Local);
                let current = match scope {
                    HarnessScope::Global => &global,
                    HarnessScope::Local => local.as_ref()?,
                };
                let state = review_state(&result, &reviews, current);
                Some(Learning {
                    result,
                    scope,
                    state,
                })
            })
            .collect())
    }

    /// Record the user's verdict on one learning. A revert first checks that
    /// every entry the learning wrote is unchanged, then rolls it back.
    ///
    /// # Errors
    ///
    /// Returns an error when the learning is unknown or already reviewed, when
    /// a revert finds a changed entry or its rollback leaves an edit
    /// unapplied, or when the verdict cannot be appended.
    pub(crate) async fn review_learning(
        &self,
        result_id: &str,
        decision: ReviewDecision,
        model: &pa_types::ai::Model,
    ) -> anyhow::Result<ReviewOutcome> {
        let learning = self
            .learnings()
            .await?
            .into_iter()
            .find(|learning| learning.result.id == result_id)
            .ok_or_else(|| anyhow::anyhow!("Learning {result_id} not found"))?;
        anyhow::ensure!(
            !matches!(learning.state, ReviewState::Kept | ReviewState::Reverted),
            "Learning {result_id} is already reviewed"
        );
        let stores = self.refinement_stores()?;
        let harness_dir = match learning.scope {
            HarnessScope::Global => stores.global_dir.clone(),
            HarnessScope::Local => local_store_dir(stores.local_dir.as_deref())?,
        };
        let (verdict, rollback) = match decision {
            ReviewDecision::Keep => (Verdict::Keep, None),
            ReviewDecision::Revert {
                failed_criteria,
                note,
            } => {
                let current = load_harness_state(&harness_dir, learning.scope);
                let changed = changed_entries(&learning.result, &current);
                anyhow::ensure!(
                    changed.is_empty(),
                    "Learning {result_id} cannot be reverted: {} changed after it",
                    changed.join(", ")
                );
                let rollback = run_refinement(
                    self.session.clone(),
                    self.agent.clone(),
                    self.compaction_flight.clone(),
                    self.refinements_in_flight.clone(),
                    self.refinement_signal(),
                    self.agent_dir.clone(),
                    RefineOptions {
                        global: learning.scope == HarnessScope::Global,
                        instructions: None,
                        rollback_id: Some(result_id.to_string()),
                    },
                    RefinementSource::User,
                    model.clone(),
                    rollback_refiner(),
                    stores,
                )
                .await?;
                let unapplied: Vec<String> = rollback
                    .result
                    .applied_edits
                    .iter()
                    .filter(|edit| !edit.applied)
                    .map(|edit| {
                        format!(
                            "{}: {}",
                            edit.id,
                            edit.error.as_deref().unwrap_or("not applied")
                        )
                    })
                    .collect();
                anyhow::ensure!(
                    unapplied.is_empty(),
                    "Rollback {} left edits unapplied: {}",
                    rollback.result.id,
                    unapplied.join("; ")
                );
                (
                    Verdict::Revert {
                        failed_criteria,
                        note,
                        rollback_id: rollback.result.id.clone(),
                    },
                    Some(rollback),
                )
            }
        };
        let record = ReviewRecord {
            result_id: result_id.to_string(),
            verdict,
            at: crate::session::manager::format_iso_now(),
        };
        append_review(&harness_dir, &record)?;
        Ok(ReviewOutcome {
            record,
            scope: learning.scope,
            rollback,
        })
    }
}

impl SessionEngine {
    /// Record the user's verdict on one learning and report it as the
    /// `learning reviewed` adoption event.
    ///
    /// # Errors
    ///
    /// Returns the review's error; nothing is reported for a refused verdict.
    pub async fn review_learning(
        &self,
        result_id: &str,
        decision: ReviewDecision,
        model: &pa_types::ai::Model,
    ) -> anyhow::Result<ReviewOutcome> {
        let outcome = self
            .session
            .review_learning(result_id, decision, model)
            .await?;
        if let Some(telemetry) = self.telemetry.as_ref() {
            telemetry.note_learning_reviewed(&outcome.record.verdict, outcome.scope);
        }
        Ok(outcome)
    }
}

impl Learning {
    /// The transcript card for this learning in a review list.
    #[must_use]
    pub fn review_message(&self) -> CustomMessage {
        let mut message = create_refinement_outcome_message(&self.result);
        message.custom_type = LEARNING_REVIEW_CUSTOM_TYPE.to_string();
        message.content = UserContent::Text(format!(
            "Learning {}: {}",
            self.result.id, self.result.summary
        ));
        if let Some(details) = message.details.as_mut() {
            details["reviewState"] = serde_json::json!(self.state);
            details["rationale"] = serde_json::json!(self.result.rationale);
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refinement::review::{Criterion, REVIEW_LOG_FILE_NAME};
    use crate::refinement::{update_harness_state, RefinementKind};
    use crate::session_engine::compaction_outcome_tests::{
        refine_plan_call, refine_test_session, session_ai_model, APPLIED_PLAN,
    };

    async fn session_with_one_learning() -> (AgentSession, tempfile::TempDir, String) {
        let (session, tmp) = refine_test_session().await;
        let learning = session
            .refine_with_refiner(
                &RefineOptions::default(),
                RefinementSource::User,
                &session_ai_model(),
                refine_plan_call(APPLIED_PLAN),
            )
            .await
            .unwrap();
        (session, tmp, learning.id)
    }

    fn local_dir(session: &AgentSession) -> std::path::PathBuf {
        session.refinement_stores().unwrap().local_dir.unwrap()
    }

    async fn state_of(session: &AgentSession, id: &str) -> ReviewState {
        session
            .learnings()
            .await
            .unwrap()
            .into_iter()
            .find(|learning| learning.result.id == id)
            .unwrap()
            .state
    }

    fn revert(criteria: Vec<Criterion>, note: &str) -> ReviewDecision {
        ReviewDecision::Revert {
            failed_criteria: criteria.try_into().unwrap(),
            note: Some(note.to_string()),
        }
    }

    #[tokio::test]
    async fn keep_records_a_verdict_once() {
        let (session, _tmp, id) = session_with_one_learning().await;
        assert_eq!(state_of(&session, &id).await, ReviewState::Pending);
        let record = session
            .review_learning(&id, ReviewDecision::Keep, &session_ai_model())
            .await
            .unwrap()
            .record;
        assert_eq!(record.verdict, Verdict::Keep);
        assert_eq!(state_of(&session, &id).await, ReviewState::Kept);
        let again = session
            .review_learning(&id, ReviewDecision::Keep, &session_ai_model())
            .await;
        assert!(
            again.is_err(),
            "a reviewed learning takes no second verdict"
        );
    }

    #[tokio::test]
    async fn revert_rolls_back_and_records_why() {
        let (session, _tmp, id) = session_with_one_learning().await;
        let outcome = session
            .review_learning(
                &id,
                revert(vec![Criterion::OneOff], "task state"),
                &session_ai_model(),
            )
            .await
            .unwrap();
        let local_dir = local_dir(&session);
        let state = load_harness_state(&local_dir, HarnessScope::Local);
        assert!(!state.entries[&RefinementKind::Memory].contains_key("m1"));
        let rollback = outcome.rollback.expect("a revert runs a rollback").result;
        assert_eq!(rollback.rollback_of.as_deref(), Some(id.as_str()));
        let learnings = session.learnings().await.unwrap();
        assert_eq!(learnings.len(), 1, "the rollback is not itself a learning");
        assert_eq!(learnings[0].state, ReviewState::Reverted);
        let log = std::fs::read_to_string(local_dir.join(REVIEW_LOG_FILE_NAME)).unwrap();
        let line: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(
            line,
            serde_json::json!({
                "result_id": id,
                "verdict": "revert",
                "failed_criteria": ["one_off"],
                "note": "task state",
                "rollback_id": rollback.id,
                "at": outcome.record.at,
            })
        );
    }

    #[tokio::test]
    async fn revert_refuses_when_the_entry_changed_after_the_learning() {
        let (session, _tmp, id) = session_with_one_learning().await;
        let local_dir = local_dir(&session);
        update_harness_state(&local_dir, HarnessScope::Local, |state| {
            let entry = state
                .entries
                .get_mut(&RefinementKind::Memory)
                .unwrap()
                .get_mut("m1")
                .unwrap();
            entry.content = "edited by the kernel".to_string();
        })
        .unwrap();
        assert_eq!(state_of(&session, &id).await, ReviewState::Stale);
        let error = session
            .review_learning(
                &id,
                revert(vec![Criterion::Inaccurate], "wrong"),
                &session_ai_model(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("memory:m1"), "{error}");
        let state = load_harness_state(&local_dir, HarnessScope::Local);
        assert_eq!(
            state.entries[&RefinementKind::Memory]["m1"].content,
            "edited by the kernel"
        );
        assert!(!local_dir.join(REVIEW_LOG_FILE_NAME).exists());
    }

    #[tokio::test]
    async fn the_refiner_sees_the_verdict() {
        let (session, _tmp, id) = session_with_one_learning().await;
        session
            .review_learning(
                &id,
                revert(vec![Criterion::WrongScope, Criterion::OneOff], "task state"),
                &session_ai_model(),
            )
            .await
            .unwrap();
        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = std::sync::Arc::clone(&prompts);
        let next_plan = refine_plan_call(r#"{"summary":"nothing","edits":[]}"#);
        session
            .refine_with_refiner(
                &RefineOptions::default(),
                RefinementSource::User,
                &session_ai_model(),
                Box::new(move |model, system, prompt| {
                    recorder.lock().unwrap().push(prompt.clone());
                    next_plan(model, system, prompt)
                }),
            )
            .await
            .unwrap();
        let prompt = prompts.lock().unwrap().pop().unwrap();
        assert!(
            prompt.contains("User review: reverted (one-off, wrong scope). Note: task state"),
            "{prompt}"
        );
    }

    /// A direct `rlm.harness` write, in the shape the kernel appends it.
    const KERNEL_WRITE: &str = r#"{"id": "refine_20261117000209897_8d62f4", "summary": "Agent created memory Prefer uv", "rationale": "Direct kernel write (rlm.harness).", "expectedOutcome": "", "appliedEdits": [{"action": "create", "kind": "memory", "id": "m_uv", "title": "Prefer uv", "after": {"id": "m_uv", "kind": "memory", "title": "Prefer uv", "content": "Use uv run for python.", "path": "general", "scope": "local", "reference": {}, "arguments": {}, "metadata": {}, "source": "agent", "created_at": "2026-11-17T00:02:09.892847+00:00", "updated_at": "2026-11-17T00:02:09.892978+00:00", "version": 1}, "applied": true}], "scope": "local"}"#;

    #[tokio::test]
    async fn a_direct_kernel_write_is_a_revertable_learning() {
        let (session, _tmp) = refine_test_session().await;
        let local_dir = local_dir(&session);
        std::fs::create_dir_all(&local_dir).unwrap();
        let state_path = crate::refinement::get_harness_state_path(&local_dir);
        let mut line: serde_json::Value = serde_json::from_str(KERNEL_WRITE).unwrap();
        line["harnessStatePath"] = serde_json::json!(state_path.to_string_lossy());
        let state = serde_json::json!({
            "schema": 1,
            "entries": {"memory": {"m_uv": line["appliedEdits"][0]["after"]}},
            "refinements": [],
        });
        std::fs::write(&state_path, state.to_string()).unwrap();
        std::fs::write(
            crate::refinement::get_refinement_history_path(&local_dir),
            format!("{line}\n"),
        )
        .unwrap();
        let id = "refine_20261117000209897_8d62f4";
        assert_eq!(state_of(&session, id).await, ReviewState::Pending);
        session
            .review_learning(
                id,
                revert(vec![Criterion::OneOff], "task state"),
                &session_ai_model(),
            )
            .await
            .unwrap();
        let state = load_harness_state(&local_dir, HarnessScope::Local);
        assert!(!state.entries[&RefinementKind::Memory].contains_key("m_uv"));
    }
}
