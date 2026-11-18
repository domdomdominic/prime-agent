use super::*;
use serde_json::json;

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn learning(id: &str, state: &str) -> Value {
    json!({
        "refinementId": id,
        "summary": format!("remember {id}"),
        "scope": "local",
        "edits": [{"action": "create", "kind": "memory", "id": "m1", "applied": true}],
        "reviewState": state,
        "rationale": "the user said so",
    })
}

fn review() -> LearningsReview {
    let data = json!({"learnings": [
        learning("r1", "pending"),
        learning("r2", "kept"),
        learning("r3", "stale"),
    ]});
    LearningsReview::new(parse_pending_learnings(&data), 40)
}

#[test]
fn only_learnings_awaiting_a_verdict_are_listed() {
    let ids: Vec<(String, bool)> = review()
        .learnings
        .into_iter()
        .map(|learning| (learning.id, learning.stale))
        .collect();
    assert_eq!(ids, [("r1".to_string(), false), ("r3".to_string(), true)]);
}

#[test]
fn a_revert_needs_a_failed_criterion_and_carries_the_note() {
    let kb = kb();
    let mut review = review();
    review.handle_key("down", &kb);
    review.handle_key("r", &kb);
    assert_eq!(review.handle_key("enter", &kb), LearningsReviewAction::None);
    assert!(review.error.is_some(), "an empty revert is refused");
    for key in ["3", "4", "3", "4", "4", "tab", "o", "k"] {
        review.handle_key(key, &kb);
    }
    assert_eq!(
        review.handle_key("enter", &kb),
        LearningsReviewAction::Review {
            refinement_id: "r3".to_string(),
            verdict: Verdict::Revert {
                failed_criteria: vec!["wrong_component"],
                note: Some("ok".to_string()),
            },
        }
    );
}

#[test]
fn keep_names_the_selected_learning_and_a_recorded_verdict_drops_it() {
    let kb = kb();
    let mut review = review();
    assert_eq!(
        review.handle_key("k", &kb),
        LearningsReviewAction::Review {
            refinement_id: "r1".to_string(),
            verdict: Verdict::Keep,
        }
    );
    review.reviewed("r1");
    assert_eq!(review.learnings.len(), 1);
    assert_eq!(review.learnings[review.selected].id, "r3");
}
