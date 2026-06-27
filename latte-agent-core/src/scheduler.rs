//! Round-robin peer discussion scheduler + H2-tagged plan.md slicing.

/// Return the substring of `plan_md` that `role_id` should see this turn.
///
/// Slice rule: every H2 section whose header starts with `<role_id> `,
/// PLUS everything before the first `## ` H2 (the initial-prompt
/// section). If no H2 header starts with `<role_id> `, return the full
/// plan_md (defensive default for unknown roles).
pub fn plan_md_slice_for(plan_md: &str, role_id: &str) -> String {
    if plan_md.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    let mut matched_any = false;
    let mut current_section: Option<(String, String)> = None;

    for line in plan_md.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            // Flush previous section, if any.
            if let Some((h, b)) = current_section.take() {
                if h.split_whitespace().next() == Some(role_id) {
                    matched_any = true;
                    out.push_str(&format!("## {}\n{}\n", h, b));
                }
            }
            current_section = Some((header.to_string(), String::new()));
        } else if let Some((_, ref mut body)) = current_section {
            body.push_str(line);
            body.push('\n');
        } else {
            // Lines before the first H2 belong to the initial-prompt prelude.
            out.push_str(line);
            out.push('\n');
        }
    }
    // Flush trailing section.
    if let Some((h, b)) = current_section {
        if h.split_whitespace().next() == Some(role_id) {
            matched_any = true;
            out.push_str(&format!("## {}\n{}\n", h, b));
        }
    }

    if !matched_any {
        return plan_md.to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_matching_h2() {
        let plan = "# Task\n\ninitial prompt here\n\n## programmer round 1\nfoo work\n\n## reviewer round 1\nbar review\n";
        let s = plan_md_slice_for(plan, "programmer");
        assert!(s.contains("initial prompt here"), "top-of-file must be included: {}", s);
        assert!(s.contains("## programmer round 1"), "own H2 must be included: {}", s);
        assert!(s.contains("foo work"), "own body must be included: {}", s);
        assert!(!s.contains("## reviewer round 1"), "other role's H2 must NOT be included: {}", s);
        assert!(!s.contains("bar review"), "other role's body must NOT be included: {}", s);
    }

    #[test]
    fn unknown_role_returns_full() {
        let plan = "# Task\n\ninit\n\n## programmer round 1\nfoo\n";
        let s = plan_md_slice_for(plan, "ghost");
        assert_eq!(s, plan);
    }

    #[test]
    fn empty_plan_returns_empty() {
        assert_eq!(plan_md_slice_for("", "programmer"), "");
    }
}