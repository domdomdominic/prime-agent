//! Learning review: the user's keep or revert verdict on each applied
//! refinement, stored append-only in `review_log.jsonl` beside the scope's
//! harness state.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{HarnessState, RefinementKind, RefinementResult};

pub const REVIEW_LOG_FILE_NAME: &str = "review_log.jsonl";

/// The fixed review criteria, each named for the way a learning fails it;
/// a revert names at least one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Criterion {
    /// It did not happen, or it is wrong or out of date.
    Inaccurate,
    /// Only relevant to the current task; it changes no future action.
    OneOff,
    /// Saved globally when it belongs to this session, or the other way
    /// around; a project-specific global entry must name the project.
    WrongScope,
    /// The wrong harness component: memory, prompt note, skill, or subagent
    /// spec, where the smallest fitting one is right.
    WrongComponent,
    /// Repeats an entry that already exists.
    Redundant,
}

impl Criterion {
    pub const ALL: [Self; 5] = [
        Self::Inaccurate,
        Self::OneOff,
        Self::WrongScope,
        Self::WrongComponent,
        Self::Redundant,
    ];

    /// A criterion from its 1-based number or its `snake_case` name.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "1" | "inaccurate" => Some(Self::Inaccurate),
            "2" | "one_off" => Some(Self::OneOff),
            "3" | "wrong_scope" => Some(Self::WrongScope),
            "4" | "wrong_component" => Some(Self::WrongComponent),
            "5" | "redundant" => Some(Self::Redundant),
            _ => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Inaccurate => "inaccurate",
            Self::OneOff => "one-off",
            Self::WrongScope => "wrong scope",
            Self::WrongComponent => "wrong component",
            Self::Redundant => "redundant",
        }
    }
}

/// The criteria a revert failed: never empty, sorted, without duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Criterion>", into = "Vec<Criterion>")]
pub struct FailedCriteria(Vec<Criterion>);

impl TryFrom<Vec<Criterion>> for FailedCriteria {
    type Error = &'static str;

    fn try_from(mut criteria: Vec<Criterion>) -> Result<Self, Self::Error> {
        criteria.sort();
        criteria.dedup();
        if criteria.is_empty() {
            return Err("a revert needs at least one failed criterion");
        }
        Ok(Self(criteria))
    }
}

impl From<FailedCriteria> for Vec<Criterion> {
    fn from(criteria: FailedCriteria) -> Self {
        criteria.0
    }
}

impl FailedCriteria {
    #[must_use]
    pub fn as_slice(&self) -> &[Criterion] {
        &self.0
    }

    /// The labels joined for display, e.g. `one-off, wrong scope`.
    #[must_use]
    pub fn labels(&self) -> String {
        self.0
            .iter()
            .map(|criterion| criterion.label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum Verdict {
    Keep,
    Revert {
        failed_criteria: FailedCriteria,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// The rollback refinement that undid the learning.
        rollback_id: String,
    },
}

/// One `review_log.jsonl` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub result_id: String,
    #[serde(flatten)]
    pub verdict: Verdict,
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewState {
    /// No verdict yet, and the entries still hold what the learning wrote.
    Pending,
    Kept,
    Reverted,
    /// No verdict, and a later change touched an entry this learning wrote.
    Stale,
}

#[must_use]
pub fn review_log_path(harness_state_dir: &Path) -> PathBuf {
    harness_state_dir.join(REVIEW_LOG_FILE_NAME)
}

/// Append one verdict to the scope's review log.
///
/// # Errors
///
/// Error when the directory cannot be created, serialization fails, or the append fails.
pub fn append_review(harness_state_dir: &Path, record: &ReviewRecord) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(harness_state_dir)?;
    let path = review_log_path(harness_state_dir);
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())?;
    Ok(path)
}

/// The verdict per learning across the given scopes' review logs; malformed
/// lines are skipped and a later line for the same learning wins.
#[derive(Debug, Default)]
pub struct ReviewLog {
    by_result: HashMap<String, ReviewRecord>,
}

impl ReviewLog {
    #[must_use]
    pub fn load<'a>(harness_state_dirs: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut by_result = HashMap::new();
        for dir in harness_state_dirs {
            let Ok(content) = std::fs::read_to_string(review_log_path(dir)) else {
                continue;
            };
            for record in content
                .lines()
                .filter_map(|line| serde_json::from_str::<ReviewRecord>(line.trim()).ok())
            {
                by_result.insert(record.result_id.clone(), record);
            }
        }
        Self { by_result }
    }

    #[must_use]
    pub fn verdict(&self, result_id: &str) -> Option<&Verdict> {
        self.by_result.get(result_id).map(|record| &record.verdict)
    }
}

/// A refinement the user reviews: it applied at least one edit and is not
/// itself a rollback.
#[must_use]
pub fn is_learning(result: &RefinementResult) -> bool {
    result.rollback_of.is_none() && result.applied_edits.iter().any(|edit| edit.applied)
}

/// The `kind:id` keys of entries whose current value differs from what this
/// learning last wrote to them (absent, for a delete).
#[must_use]
pub fn changed_entries(result: &RefinementResult, current: &HarnessState) -> Vec<String> {
    let mut written: BTreeMap<(RefinementKind, &str), Option<serde_json::Value>> = BTreeMap::new();
    for edit in result.applied_edits.iter().filter(|edit| edit.applied) {
        written.insert(
            (edit.kind, edit.id.as_str()),
            edit.after
                .as_ref()
                .and_then(|entry| serde_json::to_value(entry).ok()),
        );
    }
    written
        .into_iter()
        .filter(|((kind, id), after)| {
            let now = current
                .entries
                .get(kind)
                .and_then(|entries| entries.get(*id))
                .and_then(|entry| serde_json::to_value(entry).ok());
            &now != after
        })
        .map(|((kind, id), _)| format!("{}:{id}", super::kind_name(kind)))
        .collect()
}

/// `current` is the harness state of the learning's own scope.
#[must_use]
pub fn review_state(
    result: &RefinementResult,
    reviews: &ReviewLog,
    current: &HarnessState,
) -> ReviewState {
    match reviews.verdict(&result.id) {
        Some(Verdict::Keep) => ReviewState::Kept,
        Some(Verdict::Revert { .. }) => ReviewState::Reverted,
        None if changed_entries(result, current).is_empty() => ReviewState::Pending,
        None => ReviewState::Stale,
    }
}

/// The verdict line the refiner prompt shows under a learning.
#[must_use]
pub fn verdict_for_prompt(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Keep => "User review: kept.".to_string(),
        Verdict::Revert {
            failed_criteria,
            note,
            ..
        } => {
            let failed = failed_criteria.labels();
            match note {
                Some(note) => format!("User review: reverted ({failed}). Note: {note}"),
                None => format!("User review: reverted ({failed})."),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revert_without_a_failed_criterion_is_unrepresentable() {
        assert!(FailedCriteria::try_from(Vec::new()).is_err());
        let line = r#"{"result_id":"r1","verdict":"revert","failed_criteria":[],"rollback_id":"r2","at":"t"}"#;
        assert!(serde_json::from_str::<ReviewRecord>(line).is_err());
    }

    #[test]
    fn review_log_lines_use_the_documented_shape() {
        let record = ReviewRecord {
            result_id: "r1".to_string(),
            verdict: Verdict::Revert {
                failed_criteria: vec![Criterion::WrongScope, Criterion::OneOff, Criterion::OneOff]
                    .try_into()
                    .unwrap(),
                note: None,
                rollback_id: "r2".to_string(),
            },
            at: "2026-11-11T00:00:00Z".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            serde_json::json!({
                "result_id": "r1",
                "verdict": "revert",
                "failed_criteria": ["one_off", "wrong_scope"],
                "rollback_id": "r2",
                "at": "2026-11-11T00:00:00Z"
            })
        );
        let keep = serde_json::json!({"result_id": "r1", "verdict": "keep", "at": "t"});
        assert_eq!(
            serde_json::from_value::<ReviewRecord>(keep)
                .unwrap()
                .verdict,
            Verdict::Keep
        );
    }
}
