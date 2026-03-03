//! MCP error handling with helpful messages.
//!
//! Returns errors using MCP's `isError: true` response field with
//! human-readable messages. Each function creates a [`CallToolResult`]
//! that tools can return directly.
//!
//! Error categories match the design doc (docs/design/mcp-interface.md):
//! - `repository_not_found`: lists available repos
//! - `file_not_found`: suggests similar filenames (did-you-mean)
//! - `invalid_query`: shows position of syntax error
//! - `invalid_parameter`: shows valid range
//! - `index_building`: shows progress percentage
//! - `no_results`: suggests query refinements

use rmcp::model::{CallToolResult, Content};

/// Create an error response for when a repository is not found.
///
/// Lists available repositories so the LLM can self-correct.
///
/// # Examples
///
/// ```ignore
/// use indexrs_mcp::errors::repo_not_found;
///
/// let result = repo_not_found("foo", &["indexrs".into(), "myproject".into()]);
/// assert_eq!(result.is_error, Some(true));
/// ```
pub fn repo_not_found(repo: &str, available: &[String]) -> CallToolResult {
    let msg = if available.is_empty() {
        format!("Error: Repository \"{repo}\" not found. No repositories are currently indexed.")
    } else {
        format!(
            "Error: Repository \"{repo}\" not found. Indexed repositories: {}",
            available.join(", ")
        )
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create an error response for when a file is not found in the index.
///
/// Includes did-you-mean suggestions if similar filenames exist.
pub fn file_not_found(path: &str, suggestions: &[String]) -> CallToolResult {
    let msg = if suggestions.is_empty() {
        format!("Error: File \"{path}\" not found in the index.")
    } else if suggestions.len() == 1 {
        format!(
            "Error: File \"{path}\" not found. Did you mean \"{}\"?",
            suggestions[0]
        )
    } else {
        format!(
            "Error: File \"{path}\" not found. Similar files: {}",
            suggestions.join(", ")
        )
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create an error response for an invalid query.
///
/// The `msg` should include position info when available, e.g.
/// `"unmatched '(' at position 5"`.
pub fn invalid_query(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!("Error: Invalid query: {msg}"))])
}

/// Create an error response for an invalid parameter value.
///
/// Shows the parameter name and what went wrong, e.g.
/// `invalid_parameter("context_lines", "must be between 0 and 10, got 25")`.
pub fn invalid_parameter(param: &str, msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!(
        "Error: Invalid parameter \"{param}\": {msg}"
    ))])
}

/// Create an error response when the index is currently being built.
///
/// Shows progress percentage if available.
pub fn index_building(progress_pct: Option<f64>) -> CallToolResult {
    let msg = match progress_pct {
        Some(pct) => format!(
            "Error: Index is currently being built ({:.0}% complete). Try again shortly.",
            pct
        ),
        None => "Error: Index is currently being built. Try again shortly.".to_string(),
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create an error response for a daemon communication failure.
///
/// Used when the daemon is unreachable, times out, or returns a
/// protocol-level error — distinct from a query syntax error.
pub fn daemon_dispatch_error(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!(
        "Error: Daemon request failed: {msg}"
    ))])
}

/// Create a response for when a search returns no results.
///
/// This is NOT an error -- it's a valid empty result with suggestions.
/// Uses `CallToolResult::success` since zero results is not a failure.
pub fn no_results(query: &str, suggestions: &[String]) -> CallToolResult {
    let mut msg = format!("No matches found for \"{query}\".");
    if suggestions.is_empty() {
        msg.push_str(" Suggestions: check spelling, try a broader query, or remove filters.");
    } else {
        msg.push_str(" Suggestions: ");
        msg.push_str(&suggestions.join("; "));
        msg.push('.');
    }
    // no_results is not an error -- it's a valid empty result
    CallToolResult::success(vec![Content::text(msg)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: extract text content from a CallToolResult.
    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0]
            .raw
            .as_text()
            .expect("expected text content")
            .text
            .as_str()
    }

    // ---- repo_not_found ----

    #[test]
    fn test_repo_not_found_with_repos() {
        let result = repo_not_found("foo", &["indexrs".into(), "myproject".into()]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"foo\""), "should contain repo name");
        assert!(text.contains("indexrs"), "should list available repos");
        assert!(text.contains("myproject"));
    }

    #[test]
    fn test_repo_not_found_no_repos() {
        let result = repo_not_found("foo", &[]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("No repositories are currently indexed"));
    }

    #[test]
    fn test_repo_not_found_single_repo() {
        let result = repo_not_found("bar", &["indexrs".into()]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("Indexed repositories: indexrs"));
    }

    // ---- file_not_found ----

    #[test]
    fn test_file_not_found_single_suggestion() {
        let result = file_not_found("src/missing.rs", &["src/main.rs".into()]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"src/missing.rs\""));
        assert!(text.contains("Did you mean"));
        assert!(text.contains("src/main.rs"));
    }

    #[test]
    fn test_file_not_found_multiple_suggestions() {
        let result = file_not_found(
            "src/missing.rs",
            &["src/main.rs".into(), "src/lib.rs".into()],
        );
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("Similar files"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("src/lib.rs"));
    }

    #[test]
    fn test_file_not_found_no_suggestions() {
        let result = file_not_found("totally/unknown.rs", &[]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"totally/unknown.rs\""));
        assert!(!text.contains("Did you mean"));
        assert!(!text.contains("Similar files"));
    }

    // ---- invalid_query ----

    #[test]
    fn test_invalid_query() {
        let result = invalid_query("unmatched '(' at position 5");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("Invalid query"));
        assert!(text.contains("position 5"));
    }

    #[test]
    fn test_invalid_query_regex() {
        let result = invalid_query("invalid regex: unclosed group");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("invalid regex"));
    }

    // ---- invalid_parameter ----

    #[test]
    fn test_invalid_parameter() {
        let result = invalid_parameter("context_lines", "must be between 0 and 10, got 25");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"context_lines\""));
        assert!(text.contains("must be between 0 and 10"));
    }

    #[test]
    fn test_invalid_parameter_max_results() {
        let result = invalid_parameter("max_results", "must be between 1 and 100, got 500");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"max_results\""));
    }

    // ---- index_building ----

    #[test]
    fn test_index_building_with_progress() {
        let result = index_building(Some(45.0));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("45%"));
        assert!(text.contains("Try again shortly"));
    }

    #[test]
    fn test_index_building_no_progress() {
        let result = index_building(None);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("being built"));
        assert!(!text.contains("%"));
    }

    #[test]
    fn test_index_building_zero_progress() {
        let result = index_building(Some(0.0));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("0%"));
    }

    // ---- no_results ----

    #[test]
    fn test_no_results_default_suggestions() {
        let result = no_results("foobar", &[]);
        // no_results is NOT an error
        assert_eq!(result.is_error, Some(false));
        let text = extract_text(&result);
        assert!(text.contains("\"foobar\""));
        assert!(text.contains("check spelling"));
    }

    #[test]
    fn test_no_results_custom_suggestions() {
        let result = no_results(
            "foobar",
            &[
                "try removing the path: filter".into(),
                "use a broader query".into(),
            ],
        );
        assert_eq!(result.is_error, Some(false));
        let text = extract_text(&result);
        assert!(text.contains("try removing the path: filter"));
        assert!(text.contains("use a broader query"));
    }

    #[test]
    fn test_no_results_single_suggestion() {
        let result = no_results("xyz", &["try a shorter query".into()]);
        assert_eq!(result.is_error, Some(false));
        let text = extract_text(&result);
        assert!(text.contains("try a shorter query"));
    }

    // ---- daemon_dispatch_error ----

    #[test]
    fn test_daemon_dispatch_error() {
        let result = daemon_dispatch_error("connection refused");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("connection refused"));
        assert!(!text.contains("Invalid query"));
    }

    #[test]
    fn test_daemon_dispatch_error_timeout() {
        let result = daemon_dispatch_error("daemon did not start within timeout");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("timeout"));
    }
}
