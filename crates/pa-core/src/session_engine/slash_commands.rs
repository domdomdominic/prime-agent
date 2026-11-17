//! Session-side slash-command handling: which commands execute in the
//! session engine and how their arguments parse; the command table itself
//! is shared vocabulary in `pa_types::slash_commands` (one table serves
//! every surface).

pub use pa_types::slash_commands::{
    find_slash_command_suggestion, is_session_slash_command_name, parse_slash_command,
    BuiltinSlashCommand, ResolvedSlashCommand, SlashCommandExecution, SlashCommandRegistry,
    SESSION_SLASH_COMMAND_NAMES,
};

use crate::refinement::review::{Criterion, FailedCriteria};

/// A parsed session slash command (compact/refine/goal/autonomous).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSlashCommand {
    pub name: &'static str,
    pub args: String,
    pub text: String,
}

/// Parse a session command from input; None for non-session commands.
#[must_use]
pub fn parse_session_command(
    registry: &SlashCommandRegistry,
    text: &str,
) -> Option<SessionSlashCommand> {
    let resolved = registry.parse(text)?;
    if !is_session_slash_command_name(resolved.name) {
        return None;
    }
    Some(SessionSlashCommand {
        name: resolved.name,
        args: resolved.args.clone(),
        text: text.to_string(),
    })
}

/// Parsed `/refine` options.
#[derive(Debug, Default, PartialEq)]
pub struct RefineCommandOptions {
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
    pub global: bool,
}

/// Parse `/refine [--global] [instructions]` and `/refine rollback <id>`.
///
/// # Errors
///
/// Returns a usage-error string when `rollback` is given without a refinement id.
pub fn parse_refine_command_options(args: &str) -> Result<RefineCommandOptions, String> {
    let mut rest = args.trim();
    let mut global = false;
    if rest.starts_with("--global") && matches!(rest.as_bytes().get(8), None | Some(b' ' | b'\t')) {
        global = true;
        rest = rest["--global".len()..].trim();
    }
    if rest == "rollback" {
        return Err("Usage: /refine rollback <refinement-id>".to_string());
    }
    if let Some(tail) = rest.strip_prefix("rollback") {
        if tail.starts_with('\t') || tail.starts_with(' ') {
            let mut rollback_id = rest["rollback".len()..].trim().to_string();
            if rollback_id == "--global" {
                return Err("Usage: /refine rollback <refinement-id>".to_string());
            }
            if rollback_id.ends_with(" --global") {
                global = true;
                rollback_id = rollback_id.trim_end_matches(" --global").trim().to_string();
            }
            if rollback_id.is_empty() {
                return Err("Usage: /refine rollback <refinement-id>".to_string());
            }
            return Ok(RefineCommandOptions {
                instructions: None,
                rollback_id: Some(rollback_id),
                global,
            });
        }
    }
    Ok(RefineCommandOptions {
        instructions: (!rest.is_empty()).then(|| rest.to_string()),
        rollback_id: None,
        global,
    })
}

/// A `/refine` learning-review subcommand.
#[derive(Debug, PartialEq)]
pub enum LearningReviewCommand {
    List,
    Keep {
        id: String,
    },
    Revert {
        id: String,
        failed_criteria: FailedCriteria,
        note: Option<String>,
    },
}

const REVERT_USAGE: &str = "Usage: /refine revert <refinement-id> <criteria> [note], where criteria is a comma-separated list of 1-5 or inaccurate,one_off,wrong_scope,wrong_component,redundant";

/// Parse `/refine review`, `/refine keep <id>`, and
/// `/refine revert <id> <criteria> [note]`; `None` for every other `/refine`.
///
/// # Errors
///
/// Returns a usage-error string when `keep` or `revert` is missing its id or
/// a revert names no valid criterion.
pub fn parse_learning_review_command(args: &str) -> Option<Result<LearningReviewCommand, String>> {
    let mut words = args.split_whitespace();
    let subcommand = words.next()?;
    let id = words.next().map(str::to_string);
    match subcommand {
        "review" if id.is_none() => Some(Ok(LearningReviewCommand::List)),
        "keep" => Some(match (id, words.next()) {
            (Some(id), None) => Ok(LearningReviewCommand::Keep { id }),
            _ => Err("Usage: /refine keep <refinement-id>".to_string()),
        }),
        "revert" => {
            let parsed = id.zip(words.next()).and_then(|(id, criteria)| {
                let criteria = criteria
                    .split(',')
                    .map(Criterion::parse)
                    .collect::<Option<Vec<_>>>()?;
                let note = words.collect::<Vec<_>>().join(" ");
                Some(LearningReviewCommand::Revert {
                    id,
                    failed_criteria: FailedCriteria::try_from(criteria).ok()?,
                    note: (!note.is_empty()).then_some(note),
                })
            });
            Some(parsed.ok_or_else(|| REVERT_USAGE.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_review_commands_parse() {
        assert_eq!(
            [
                parse_learning_review_command("review"),
                parse_learning_review_command("keep refine_1"),
                parse_learning_review_command("revert refine_1 2,wrong_scope too narrow"),
                parse_learning_review_command("revert refine_1 9"),
                parse_learning_review_command("review the last edit"),
            ],
            [
                Some(Ok(LearningReviewCommand::List)),
                Some(Ok(LearningReviewCommand::Keep {
                    id: "refine_1".to_string()
                })),
                Some(Ok(LearningReviewCommand::Revert {
                    id: "refine_1".to_string(),
                    failed_criteria: FailedCriteria::try_from(vec![
                        Criterion::OneOff,
                        Criterion::WrongScope
                    ])
                    .unwrap(),
                    note: Some("too narrow".to_string()),
                })),
                Some(Err(REVERT_USAGE.to_string())),
                None,
            ]
        );
    }

    #[test]
    fn refine_options_parsing() {
        assert_eq!(
            parse_refine_command_options("do the thing").unwrap(),
            RefineCommandOptions {
                instructions: Some("do the thing".to_string()),
                rollback_id: None,
                global: false,
            }
        );
        assert_eq!(
            parse_refine_command_options("").unwrap(),
            RefineCommandOptions::default()
        );
        assert_eq!(
            parse_refine_command_options("--global tweak").unwrap(),
            RefineCommandOptions {
                instructions: Some("tweak".to_string()),
                rollback_id: None,
                global: true,
            }
        );
        assert_eq!(
            parse_refine_command_options("rollback ref_123").unwrap(),
            RefineCommandOptions {
                instructions: None,
                rollback_id: Some("ref_123".to_string()),
                global: false,
            }
        );
        assert!(
            parse_refine_command_options("--global rollback ref_123")
                .unwrap()
                .global
        );
        assert_eq!(
            parse_refine_command_options("rollback").unwrap_err(),
            "Usage: /refine rollback <refinement-id>"
        );
        assert_eq!(
            parse_refine_command_options("rollback ").unwrap_err(),
            "Usage: /refine rollback <refinement-id>"
        );
    }

    #[test]
    fn session_command_extraction() {
        let registry = SlashCommandRegistry::builtin();
        let command = parse_session_command(&registry, "/compact focus on tests").unwrap();
        assert_eq!(command.name, "compact");
        assert_eq!(command.args, "focus on tests");
        assert!(parse_session_command(&registry, "/model").is_none());
        assert!(parse_session_command(&registry, "/unknown x").is_none());
    }
}
