use crate::history;
use crate::state::AppState;

/// How a known command is executed, decided per-command in `COMMANDS` so
/// the choice is explicit and centralized rather than an implicit rule
/// scattered across routing/worker code.
///
/// `Sync` and `Queued` each bundle two behaviors together (not independent
/// flags, since no command has needed the in-between case):
/// - `Sync`: runs immediately on the transport's own task, bypassing
///   the `pending`/`running` queue entirely, and is *not* recorded in
///   `job_history`. For fast, read-only commands with no Claude/git
///   involvement -- there's no reason to make `/status` wait behind an
///   in-flight ingest job, and no reason to give every call a durable
///   history record.
/// - `Queued`: goes through `Queue::enqueue` and the single sequential
///   worker exactly like an ingest job, and is recorded in `job_history`.
///   Used by every command that invokes Claude or touches the notes git
///   repo (`ingest`, `fact`, `todo`, `goal`), to preserve the "no
///   concurrent Claude runs against the same working directory" guardrail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    Sync,
    Queued,
}

pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub mode: CommandMode,
}

/// The command registry. Every known command name must appear here so the
/// unknown-command error message and the routing check (`mode`) stay in
/// sync by construction.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "Show available commands",
        mode: CommandMode::Sync,
    },
    CommandSpec {
        name: "status",
        description: "Show queue depth and the last processed note",
        mode: CommandMode::Sync,
    },
    CommandSpec {
        name: "history",
        description: "Show recent job history (usage: /history [limit])",
        mode: CommandMode::Sync,
    },
    CommandSpec {
        name: "ingest",
        description: "Ingest a link as a note, or pass text straight to the agent (usage: /ingest <link or text>)",
        mode: CommandMode::Queued,
    },
    CommandSpec {
        name: "fact",
        description: "Log an accomplishment to the bitacora, optionally with an attached file (usage: /fact <description>)",
        mode: CommandMode::Queued,
    },
    CommandSpec {
        name: "todo",
        description: "Manage TODOs (usage: /todo add <text> #tag1 #tag2 | /todo list [open|close] | /todo resolve <key or #> | /todo reopen <key or #> | /todo link <key or #> <goal key>)",
        mode: CommandMode::Queued,
    },
    CommandSpec {
        name: "goal",
        description: "Manage goals (usage: /goal add <text> #tag1 #tag2 | /goal list [open|close] | /goal achieve <key or #> | /goal reopen <key or #>)",
        mode: CommandMode::Queued,
    },
    CommandSpec {
        name: "align",
        description: "Link TODOs to matching open goals by shared tag and show the alignment report",
        mode: CommandMode::Queued,
    },
    CommandSpec {
        name: "demonstrate",
        description: "Link bitacora facts to open goals they demonstrate (usage: /demonstrate [all|YYYY-Q1..4], default: current quarter)",
        mode: CommandMode::Queued,
    },
];

/// Looks up a known command's execution mode. `None` means unknown command.
pub fn mode(name: &str) -> Option<CommandMode> {
    COMMANDS.iter().find(|c| c.name == name).map(|c| c.mode)
}

fn command_list() -> String {
    COMMANDS
        .iter()
        .map(|c| format!("/{} - {}", c.name, c.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/ingest` with no argument is a usage error, not an empty job: shared
/// between `telegram::handle_update`/`matrix::handle_message` (which replies immediately and never
/// enqueues, the same way an unknown command never enqueues) and any test
/// asserting on that reply text.
pub const INGEST_USAGE: &str = "Usage: /ingest <link or text>";

/// `/fact` with neither a description nor an attached file is a usage
/// error, not an empty job -- same shared-usage-reply pattern as
/// `INGEST_USAGE` (see `telegram::handle_update`/`matrix::handle_message`).
pub const FACT_USAGE: &str = "Usage: /fact <description> (a file can be attached too)";

pub fn unknown_command_reply(attempted: &str) -> String {
    format!(
        "Unknown command: /{attempted}\n\nAvailable commands:\n{}",
        command_list()
    )
}

/// Runs a known command. Only reachable for commands `mode()` already
/// approved (routing runs/enqueues only known commands), so the fallback
/// arm is a defensive log-and-reply rather than a panic.
///
/// `ingest`, `fact`, `todo`, `goal`, `align`, and `demonstrate` are
/// deliberately absent from this match: all six are `Queued` commands that
/// touch the notes git repo (`ingest`/`fact` always invoke Claude, `align`
/// writes TODO->goal links, `demonstrate` writes fact->goal links and
/// invokes Claude too when semantic matching is on), so
/// `worker::process_job` special-cases them ahead of this generic dispatch
/// -- the same way `JobKind::Ingest` never comes through here either. This
/// function only ever runs for the plain string-reply commands.
pub async fn dispatch(name: &str, args: &[String], state: &AppState) -> String {
    match name {
        "help" => cmd_help(),
        "status" => cmd_status(args, state).await,
        "history" => cmd_history(args, state).await,
        other => {
            tracing::error!(command = other, "dispatch called with unrecognized command; this should be unreachable because routing only runs/enqueues known commands");
            unknown_command_reply(other)
        }
    }
}

fn cmd_help() -> String {
    format!(
        "Available commands:\n{}\n\nAny message without a leading / is filed away as a note. \
         Any #tag word in the message (trailing punctuation like a comma or period is \
         ignored) is added to the note's tags alongside whatever tags Claude infers; \
         a number-only tag like #2026 is left as plain text.",
        command_list()
    )
}

async fn cmd_status(_args: &[String], state: &AppState) -> String {
    let depth = match state.queue.pending_depth() {
        Ok(depth) => depth.to_string(),
        Err(err) => {
            tracing::error!(%err, "failed to read pending queue depth");
            "unknown (error reading queue)".to_string()
        }
    };
    format!(
        "Version: {}\nQueue depth: {depth}\nLast processed note: {}",
        env!("CARGO_PKG_VERSION"),
        state.last_note_summary()
    )
}

async fn cmd_history(args: &[String], state: &AppState) -> String {
    let limit = args
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
        .min(100); // Cap at 100 to avoid huge messages

    let records = match history::query_recent(&state.queue.db, limit) {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(%err, "failed to query job history");
            return format!("Failed to read job history: {err}");
        }
    };

    if records.is_empty() {
        return "No job history found.".to_string();
    }

    let mut lines = vec![format!("Last {} jobs:\n", records.len())];

    for record in records {
        let status_icon = match record.status {
            history::JobStatus::Success => "✓",
            history::JobStatus::Failed => "✗",
        };

        let kind_display = match &record.kind {
            history::JobKindSummary::Ingest => "ingest".to_string(),
            history::JobKindSummary::Command { name } => format!("/{name}"),
        };

        let timestamp = record.completed_at.format("%Y-%m-%d %H:%M:%S").to_string();

        let tokens_display = match &record.tokens {
            Some(t) => format!(
                " | {}↓ {}↑ (cache: {}w/{}r)",
                t.input, t.output, t.cache_creation, t.cache_read
            ),
            None => String::new(),
        };

        let error_display = match &record.error_message {
            Some(err) => format!("\n  Error: {}", err.lines().next().unwrap_or(err)),
            None => String::new(),
        };

        lines.push(format!(
            "{} {} | {} | {}{}{}",
            status_icon, record.job_id, kind_display, timestamp, tokens_display, error_display
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_command_is_recognized() {
        assert!(mode("status").is_some());
    }

    #[test]
    fn unknown_command_is_not_recognized() {
        assert!(mode("nonexistent").is_none());
    }

    #[test]
    fn unknown_command_reply_lists_available_commands() {
        let reply = unknown_command_reply("bogus");
        assert!(reply.contains("Unknown command: /bogus"));
        assert!(reply.contains("/status"));
    }

    #[test]
    fn help_is_a_known_command() {
        assert!(mode("help").is_some());
    }

    #[test]
    fn fact_is_a_queued_command() {
        assert_eq!(mode("fact"), Some(CommandMode::Queued));
    }

    #[test]
    fn todo_is_a_queued_command() {
        assert_eq!(mode("todo"), Some(CommandMode::Queued));
    }

    #[test]
    fn goal_is_a_queued_command() {
        assert_eq!(mode("goal"), Some(CommandMode::Queued));
    }

    #[test]
    fn align_is_a_queued_command() {
        assert_eq!(mode("align"), Some(CommandMode::Queued));
    }

    #[test]
    fn demonstrate_is_a_queued_command() {
        assert_eq!(mode("demonstrate"), Some(CommandMode::Queued));
    }

    #[test]
    fn help_lists_all_commands() {
        let reply = cmd_help();
        for c in COMMANDS {
            assert!(
                reply.contains(&format!("/{}", c.name)),
                "missing /{} in help text",
                c.name
            );
        }
    }

    #[test]
    fn mode_returns_none_for_unknown_command() {
        assert_eq!(mode("nonexistent"), None);
    }

    #[test]
    fn mode_returns_registered_mode_for_known_commands() {
        for c in COMMANDS {
            assert_eq!(mode(c.name), Some(c.mode));
        }
    }
}
