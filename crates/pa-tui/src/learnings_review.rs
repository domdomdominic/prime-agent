//! The `/refine review` view: each learning awaiting a verdict, its change
//! card and rationale, and the keep or revert decision. A revert names at
//! least one review criterion it failed and may carry a note.

use serde_json::Value;

use crate::chat::{ChatEntry, Detail};
use crate::custom_message::refinement::{learning_review_entries, render_refinement_outcome};
use crate::custom_message::RefinementOutcomeRow;
use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{hint_row, menu_row, rule_row, search_field_lines, MenuSegment};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::{truncate_line, wrap_text};
use crate::{Line, Span};

/// The review criteria in key order (`1`-`5`): the wire name and its label.
const CRITERIA: [(&str, &str); 5] = [
    ("inaccurate", "inaccurate"),
    ("one_off", "one-off"),
    ("wrong_scope", "wrong scope"),
    ("wrong_component", "wrong component"),
    ("redundant", "redundant"),
];

/// The most learning rows the list shows above the selected card.
const MAX_LIST_ROWS: usize = 5;

/// One learning awaiting a verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingLearning {
    pub id: String,
    pub summary: String,
    pub scope: String,
    /// A later change touched an entry this learning wrote, so a revert is refused.
    pub stale: bool,
    pub rationale: String,
    card: RefinementOutcomeRow,
}

/// The learnings from a `get_learnings` response that still await a verdict.
#[must_use]
pub fn parse_pending_learnings(data: &Value) -> Vec<PendingLearning> {
    let text = |details: &Value, key: &str| {
        details
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    data.get("learnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|details| {
            let stale = match details.get("reviewState").and_then(Value::as_str)? {
                "pending" => false,
                "stale" => true,
                _ => return None,
            };
            let summary = text(details, "summary");
            let message = serde_json::json!({ "content": summary });
            let Ok([ChatEntry::RefinementOutcome(card)]) =
                <[ChatEntry; 1]>::try_from(learning_review_entries(&message, details))
            else {
                return None;
            };
            Some(PendingLearning {
                id: text(details, "refinementId"),
                summary,
                scope: text(details, "scope"),
                stale,
                rationale: text(details, "rationale"),
                card: *card,
            })
        })
        .collect()
}

/// A verdict the caller sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    Revert {
        failed_criteria: Vec<&'static str>,
        note: Option<String>,
    },
}

/// One key press while the view is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningsReviewAction {
    Close,
    Review {
        refinement_id: String,
        verdict: Verdict,
    },
    None,
}

#[derive(Debug, Clone)]
enum Mode {
    Browse,
    Revert {
        failed: [bool; CRITERIA.len()],
        note: SearchInput,
        note_focused: bool,
    },
}

/// The `/refine review` view.
#[derive(Debug)]
pub struct LearningsReview {
    learnings: Vec<PendingLearning>,
    selected: usize,
    mode: Mode,
    error: Option<String>,
    viewport_rows: usize,
}

impl LearningsReview {
    #[must_use]
    pub fn new(learnings: Vec<PendingLearning>, viewport_rows: usize) -> Self {
        Self {
            learnings,
            selected: 0,
            mode: Mode::Browse,
            error: None,
            viewport_rows,
        }
    }

    /// The daemon recorded the verdict: drop the learning and return to browsing.
    pub fn reviewed(&mut self, refinement_id: &str) {
        self.learnings
            .retain(|learning| learning.id != refinement_id);
        self.selected = self.selected.min(self.learnings.len().saturating_sub(1));
        self.mode = Mode::Browse;
        self.error = None;
    }

    /// The daemon refused the verdict; the revert form keeps its input.
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.learnings.is_empty()
    }

    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> LearningsReviewAction {
        if key == "ctrl+c" {
            return LearningsReviewAction::Close;
        }
        let Some(learning) = self.learnings.get(self.selected) else {
            return LearningsReviewAction::Close;
        };
        let refinement_id = learning.id.clone();
        match &mut self.mode {
            Mode::Browse => {
                if kb.matches(key, "tui.select.cancel") {
                    return LearningsReviewAction::Close;
                }
                if kb.matches(key, "tui.select.up") {
                    self.selected = self.selected.saturating_sub(1);
                } else if kb.matches(key, "tui.select.down") {
                    self.selected = (self.selected + 1).min(self.learnings.len() - 1);
                } else if key == "k" {
                    return LearningsReviewAction::Review {
                        refinement_id,
                        verdict: Verdict::Keep,
                    };
                } else if key == "r" {
                    self.mode = Mode::Revert {
                        failed: [false; CRITERIA.len()],
                        note: SearchInput::new(),
                        note_focused: false,
                    };
                }
                self.error = None;
                LearningsReviewAction::None
            }
            Mode::Revert {
                failed,
                note,
                note_focused,
            } => {
                if kb.matches(key, "tui.select.cancel") {
                    self.mode = Mode::Browse;
                    self.error = None;
                } else if key == "tab" {
                    *note_focused = !*note_focused;
                } else if kb.matches(key, "tui.select.confirm") {
                    let failed_criteria: Vec<&'static str> = CRITERIA
                        .iter()
                        .zip(failed.iter())
                        .filter(|(_, failed)| **failed)
                        .map(|((name, _), _)| *name)
                        .collect();
                    if failed_criteria.is_empty() {
                        self.error = Some("Mark at least one reason (1-5).".to_string());
                        return LearningsReviewAction::None;
                    }
                    let note = note.value().trim();
                    return LearningsReviewAction::Review {
                        refinement_id,
                        verdict: Verdict::Revert {
                            failed_criteria,
                            note: (!note.is_empty()).then(|| note.to_string()),
                        },
                    };
                } else if *note_focused {
                    note.handle_key(key, kb);
                } else if let Some(index) = key
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .filter(|index| *index < CRITERIA.len())
                {
                    failed[index] = !failed[index];
                }
                LearningsReviewAction::None
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Mode::Revert {
            note,
            note_focused: true,
            ..
        } = &mut self.mode
        {
            note.paste(text);
        }
    }

    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines = vec![rule_row(theme, width), self.title_row(theme), Vec::new()];
        let Some(learning) = self.learnings.get(self.selected) else {
            lines.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Muted, "No learnings to review.".to_string()),
            ]);
            lines.push(Vec::new());
            lines.push(hint_row(theme, width, "Esc close"));
            return lines;
        };
        let footer = self.footer_lines(theme, width, kb);
        let start = self
            .selected
            .saturating_sub(MAX_LIST_ROWS - 1)
            .min(self.learnings.len().saturating_sub(MAX_LIST_ROWS));
        for (index, row) in self
            .learnings
            .iter()
            .enumerate()
            .skip(start)
            .take(MAX_LIST_ROWS)
        {
            let state = if row.stale {
                MenuSegment::themed(ThemeColor::Warning, "stale")
            } else {
                MenuSegment::muted("pending")
            };
            lines.push(menu_row(
                theme,
                width,
                vec![Span::raw(
                    crate::menu_panel::scrub_controls(&row.summary).replace('\n', " "),
                )],
                &[MenuSegment::muted(&row.scope), state],
                index == self.selected,
            ));
        }
        let mut detail = render_refinement_outcome(&learning.card, Detail::All, theme, width);
        if !learning.rationale.trim().is_empty() {
            detail.push(Vec::new());
            let why = crate::menu_panel::scrub_controls(&format!("Why: {}", learning.rationale));
            for line in wrap_text(&why, width.saturating_sub(4).max(10)) {
                let mut row = vec![Span::raw("  ")];
                row.extend(
                    line.into_iter()
                        .map(|span| Span::styled(span.content, theme.fg_style(ThemeColor::Muted))),
                );
                detail.push(row);
            }
        }
        if learning.stale {
            detail.push(Vec::new());
            detail.push(vec![
                Span::raw("  "),
                theme.fg_span(
                    ThemeColor::Warning,
                    "An entry changed after this learning, so it can be kept but not reverted."
                        .to_string(),
                ),
            ]);
        }
        let budget = self
            .viewport_rows
            .saturating_sub(lines.len() + footer.len());
        if detail.len() > budget {
            detail.truncate(budget.saturating_sub(1));
            detail.push(vec![
                theme.fg_span(ThemeColor::Dim, "  \u{2026}".to_string())
            ]);
        }
        lines.extend(detail);
        lines.extend(footer);
        lines
            .into_iter()
            .take(self.viewport_rows.max(1))
            .map(|line| truncate_line(&line, width, ""))
            .collect()
    }

    fn title_row(&self, theme: &Theme) -> Line {
        let stale = self
            .learnings
            .iter()
            .filter(|learning| learning.stale)
            .count();
        let mut row = vec![
            Span::raw("  "),
            theme.fg_span(ThemeColor::Text, "Learnings to review".to_string()),
            theme.fg_span(
                ThemeColor::Muted,
                format!(" \u{b7} {} pending", self.learnings.len() - stale),
            ),
        ];
        if stale > 0 {
            row.push(theme.fg_span(ThemeColor::Warning, format!(" \u{b7} {stale} stale")));
        }
        row
    }

    /// The error row, the revert form when open, and the key hint.
    fn footer_lines(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(error) = &self.error {
            lines.push(Vec::new());
            lines.push(vec![
                Span::raw("  "),
                theme.fg_span(
                    ThemeColor::Error,
                    crate::menu_panel::scrub_controls(error).replace('\n', " "),
                ),
            ]);
        }
        let key = |binding: &str, fallback: &str| {
            kb.first_key(binding).map_or_else(
                || fallback.to_string(),
                |key| crate::keybindings::format_key_text(&key),
            )
        };
        let hint = match &self.mode {
            Mode::Browse => format!(
                "{}/{} move \u{b7} k keep \u{b7} r revert \u{b7} {} close",
                key("tui.select.up", "\u{2191}"),
                key("tui.select.down", "\u{2193}"),
                key("tui.select.cancel", "Esc"),
            ),
            Mode::Revert {
                failed,
                note,
                note_focused,
            } => {
                lines.push(Vec::new());
                let mut criteria = vec![
                    Span::raw("  "),
                    theme.fg_span(ThemeColor::Text, "Why:".to_string()),
                ];
                for (index, ((_, label), failed)) in CRITERIA.iter().zip(failed).enumerate() {
                    let (mark, color) = if *failed {
                        ("\u{25a0}", ThemeColor::Error)
                    } else {
                        ("\u{25a1}", ThemeColor::Muted)
                    };
                    criteria.push(theme.fg_span(color, format!("  {mark} {} {label}", index + 1)));
                }
                lines.push(criteria);
                lines.extend(search_field_lines(
                    theme,
                    width,
                    note.value(),
                    note.cursor(),
                    *note_focused,
                    "Optional note (Tab to type)",
                ));
                format!(
                    "1-5 toggle \u{b7} Tab note \u{b7} {} revert \u{b7} {} cancel",
                    key("tui.select.confirm", "Enter"),
                    key("tui.select.cancel", "Esc"),
                )
            }
        };
        lines.push(Vec::new());
        lines.push(hint_row(theme, width, &hint));
        lines
    }
}

#[cfg(test)]
mod tests;
