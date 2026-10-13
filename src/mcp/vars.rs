//! `${NAME}` expansion for MCP server definitions.
//!
//! Values come first from roc's own session variables (`ROC_WORKDIR`,
//! `ROC_SESSION_ID`, …) and then from the host environment. Expansion
//! reports which variables were missing so callers can drop a header,
//! skip an env entry or warn, instead of sending `Bearer ` to a server.

/// The result of expanding one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The text with every known variable substituted (unknown ones become empty).
    pub text: String,
    /// Variables that were referenced but unset or empty.
    pub missing: Vec<String>,
}

/// True for a valid environment variable name.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn lookup(name: &str, extra: &[(&str, String)]) -> Option<String> {
    if let Some((_, v)) = extra.iter().find(|(k, _)| *k == name) {
        return Some(v.clone());
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Expands `${NAME}` references in `s`.
pub fn expand(s: &str, extra: &[(&str, String)]) -> Expanded {
    let mut text = String::new();
    let mut missing = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        text.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                let name = &rest[i + 2..i + 2 + j];
                match lookup(name, extra) {
                    Some(v) => text.push_str(&v),
                    None => missing.push(name.to_string()),
                }
                rest = &rest[i + 3 + j..];
            }
            None => {
                text.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    text.push_str(rest);
    Expanded { text, missing }
}

/// The variable name when `s` is exactly one reference (`${NAME}`).
pub fn whole_ref(s: &str) -> Option<&str> {
    let name = s.strip_prefix("${")?.strip_suffix('}')?;
    valid_name(name).then_some(name)
}

/// Every variable referenced in `s`.
pub fn refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push(rest[i + 2..i + 2 + j].to_string());
                rest = &rest[i + 3 + j..];
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_reports_missing_variables() {
        // SAFETY: test-local variable names.
        unsafe { std::env::set_var("ROC_TEST_TOKEN_X", "abc") };
        unsafe { std::env::set_var("ROC_TEST_EMPTY_Z", "") };
        let e = expand("Bearer ${ROC_TEST_TOKEN_X}", &[]);
        assert_eq!(e.text, "Bearer abc");
        assert!(e.missing.is_empty());
        let e = expand("${ROC_TEST_UNSET_Y}x${ROC_TEST_EMPTY_Z}", &[]);
        assert_eq!(e.text, "x");
        assert_eq!(e.missing, vec!["ROC_TEST_UNSET_Y", "ROC_TEST_EMPTY_Z"]);
        assert_eq!(expand("no refs", &[]).text, "no refs");
        assert_eq!(expand("broken ${X", &[]).text, "broken ${X");
        let e = expand("--root ${ROC_WORKDIR}", &[("ROC_WORKDIR", "/Users/a/p".into())]);
        assert_eq!(e.text, "--root /Users/a/p");
    }

    #[test]
    fn whole_references() {
        assert_eq!(whole_ref("${FIGMA_API_KEY}"), Some("FIGMA_API_KEY"));
        assert_eq!(whole_ref("Bearer ${X}"), None);
        assert_eq!(whole_ref("${1X}"), None);
        assert_eq!(whole_ref("${A-B}"), None);
        assert_eq!(refs("a ${X} b ${Y_2}"), vec!["X", "Y_2"]);
    }
}
