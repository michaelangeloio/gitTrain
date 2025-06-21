pub fn sanitize_branch_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            ' ' => '-',
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase()
}

pub fn get_current_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

pub fn create_backup_name(prefix: &str) -> String {
    format!("{}_backup_{}", prefix, get_current_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_branch_name() {
        assert_eq!(sanitize_branch_name("Feature Branch"), "feature-branch");
        assert_eq!(sanitize_branch_name("fix/bug#123"), "fix_bug_123");
        assert_eq!(sanitize_branch_name("--start--"), "start");
    }
}
