//! REPL line parser for `latte-agent chat --task-id X`.
//!
//! Classifies each line into one of four actions:
//! - `Empty` — ignore
//! - `Cmd { name }` — built-in REPL command (`/pause`, `/resume`, `/roles`, `/quit`)
//! - `RoleInject { role_id, message }` — `@role-id ...` (routed to specialist's next turn)
//! - `ManagerInput { message }` — anything else (default manager user message)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplInput {
    Empty,
    Cmd { name: String },
    RoleInject { role_id: String, message: String },
    ManagerInput { message: String },
}

impl ReplInput {
    pub fn role_id(&self) -> Option<&str> {
        match self {
            ReplInput::RoleInject { role_id, .. } => Some(role_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplParseError {
    UnknownRole(String),
    MalformedAtLine,
}

/// Canonical list of role ids the REPL `@role-id` parser will route to.
///
/// Sourced from the design spec §4.7 — the set of specialist roles the
/// manager can delegate to (plus `manager` itself). Anything outside
/// this set returns `ReplParseError::UnknownRole`.
pub const KNOWN_ROLES: &[&str] = &[
    "manager",
    "programmer",
    "architect",
    "reviewer",
    "tester",
    "security",
    "devops",
    "designer",
    "tech_writer",
    "pm",
];

/// Parse one line of REPL input.
pub fn parse_repl_line(line: &str) -> Result<ReplInput, ReplParseError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(ReplInput::Empty);
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        if name.is_empty() || !is_valid_role_id(&name) {
            return Err(ReplParseError::MalformedAtLine);
        }
        return Ok(ReplInput::Cmd { name });
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut id_end = rest.len();
        for (i, c) in rest.char_indices() {
            if c.is_whitespace() {
                id_end = i;
                break;
            }
        }
        let role_id = rest[..id_end].to_string();
        if !is_valid_role_id(&role_id)
            || (!role_id.contains('-')
                && !role_id.contains('_')
                && !KNOWN_ROLES.contains(&role_id.as_str()))
        {
            return Err(ReplParseError::UnknownRole(role_id));
        }
        let after_id = &rest[id_end..];
        let message = after_id.trim_start().to_string();
        return Ok(ReplInput::RoleInject { role_id, message });
    }
    Ok(ReplInput::ManagerInput { message: trimmed.to_string() })
}

fn is_valid_role_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line() {
        assert_eq!(parse_repl_line("").unwrap(), ReplInput::Empty);
        assert_eq!(parse_repl_line("   ").unwrap(), ReplInput::Empty);
        assert_eq!(parse_repl_line("\t\n").unwrap(), ReplInput::Empty);
    }

    #[test]
    fn slash_pause() {
        assert_eq!(
            parse_repl_line("/pause").unwrap(),
            ReplInput::Cmd { name: "pause".to_string() }
        );
    }

    #[test]
    fn slash_resume() {
        assert_eq!(
            parse_repl_line("/resume").unwrap(),
            ReplInput::Cmd { name: "resume".to_string() }
        );
    }

    #[test]
    fn at_sign_routes_to_named_role() {
        let out = parse_repl_line("@programmer 先看 src/db/connection.rs").unwrap();
        match out {
            ReplInput::RoleInject { role_id, message } => {
                assert_eq!(role_id, "programmer");
                assert_eq!(message, "先看 src/db/connection.rs");
            }
            other => panic!("expected RoleInject, got {:?}", other),
        }
    }

    #[test]
    fn at_sign_with_dash() {
        let out = parse_repl_line("@senior-dev foo").unwrap();
        assert_eq!(out.role_id(), Some("senior-dev"));
    }

    #[test]
    fn at_sign_unknown_role_errors() {
        let err = parse_repl_line("@ghost foo").unwrap_err();
        assert_eq!(err, ReplParseError::UnknownRole("ghost".to_string()));
    }

    #[test]
    fn plain_text_routes_to_manager() {
        let out = parse_repl_line("look at foo.rs").unwrap();
        assert_eq!(out, ReplInput::ManagerInput { message: "look at foo.rs".to_string() });
    }
}
