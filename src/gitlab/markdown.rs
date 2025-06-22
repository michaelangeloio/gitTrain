use crate::gitlab::api::MergeRequest;
use crate::stack::types::Stack;
use std::collections::HashMap;

const STACK_TABLE_START: &str = "<!-- git-train-stack-start -->";
const STACK_TABLE_END: &str = "<!-- git-train-stack-end -->";

pub fn build_stack_table(stack: &Stack, mrs: &HashMap<u64, MergeRequest>) -> String {
    let mut table = String::new();

    table.push_str(STACK_TABLE_START);
    table.push_str("\n\n");
    table.push_str("### MR Train\n\n");
    table.push_str("| Position | Branch | Merge Request |\n");
    table.push_str("|---|---|---|\n");

    // Collect all branches in hierarchical order (depth-first traversal)
    let branches_in_order = collect_branches_in_order(stack);

    for (i, branch) in branches_in_order.iter().enumerate() {
        let position = format!("#{}", i + 1);

        let mr_link = if let Some(iid) = branch.mr_iid {
            if let Some(mr) = mrs.get(&iid) {
                // Append '+' to the URL to get a rich link in GitLab
                format!("[{}]({}+)", mr.title, mr.web_url)
            } else {
                "N/A (MR not found)".to_string()
            }
        } else {
            "N/A".to_string()
        };

        table.push_str(&format!(
            "| {} | `{}` | {} |\n",
            position, branch.name, mr_link
        ));
    }

    if branches_in_order.is_empty() {
        table.push_str("| | | |\n")
    }

    table.push('\n');
    table.push_str("---\n");
    table.push_str("*Created by [gitTrain](https://github.com/michaelangeloio/gitTrain)*\n\n");
    table.push_str(STACK_TABLE_END);

    table
}

/// Collect all branches in the stack in hierarchical order
/// This performs a depth-first traversal starting from branches that have the base branch as parent
fn collect_branches_in_order(stack: &Stack) -> Vec<crate::stack::types::StackBranch> {
    let mut result = Vec::new();
    let mut visited = std::collections::HashSet::new();

    // Build a parent -> children mapping for efficient traversal
    let mut hierarchy: HashMap<String, Vec<String>> = HashMap::new();
    for (branch_name, branch) in &stack.branches {
        if let Some(parent) = &branch.parent {
            hierarchy
                .entry(parent.clone())
                .or_default()
                .push(branch_name.clone());
        }
    }

    // Sort children by name for consistent ordering
    for children in hierarchy.values_mut() {
        children.sort();
    }

    // Start traversal from branches that have the base branch as parent
    if let Some(root_branches) = hierarchy.get(&stack.base_branch) {
        for root_branch in root_branches {
            collect_branch_recursive(stack, &hierarchy, root_branch, &mut result, &mut visited);
        }
    }

    // Also include any branches that might not have been visited
    // (in case of disconnected branches or circular references)
    for (branch_name, branch) in &stack.branches {
        if !visited.contains(branch_name) {
            result.push(branch.clone());
        }
    }

    result
}

/// Recursively collect branches in depth-first order
fn collect_branch_recursive(
    stack: &Stack,
    hierarchy: &HashMap<String, Vec<String>>,
    branch_name: &str,
    result: &mut Vec<crate::stack::types::StackBranch>,
    visited: &mut std::collections::HashSet<String>,
) {
    // Avoid infinite loops
    if visited.contains(branch_name) {
        return;
    }

    if let Some(branch) = stack.branches.get(branch_name) {
        visited.insert(branch_name.to_string());
        result.push(branch.clone());

        // Recursively collect children
        if let Some(children) = hierarchy.get(branch_name) {
            for child in children {
                collect_branch_recursive(stack, hierarchy, child, result, visited);
            }
        }
    }
}

pub fn update_description(current_description: &Option<String>, new_table: &str) -> String {
    let current_desc = current_description.as_deref().unwrap_or("").trim();

    if let (Some(start_idx), Some(end_idx)) = (
        current_desc.find(STACK_TABLE_START),
        current_desc.find(STACK_TABLE_END),
    ) {
        // Table exists, replace it
        let mut new_desc = String::new();
        new_desc.push_str(&current_desc[..start_idx]);
        new_desc.push_str(new_table);
        new_desc.push_str(&current_desc[end_idx + STACK_TABLE_END.len()..]);
        new_desc.trim().to_string()
    } else {
        // Table does not exist, append it
        if current_desc.is_empty() {
            new_table.to_string()
        } else {
            format!("{}\n\n{}", current_desc, new_table)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::api::MergeRequest;
    use crate::stack::types::{Stack, StackBranch};
    use chrono::Utc;

    fn create_test_stack_and_mrs() -> (Stack, HashMap<u64, MergeRequest>) {
        let mut branches = HashMap::new();
        branches.insert(
            "feature-1".to_string(),
            StackBranch {
                name: "feature-1".to_string(),
                parent: Some("main".to_string()),
                children: vec!["feature-2".to_string()],
                commit_hash: "hash1".to_string(),
                mr_iid: Some(101),
                mr_title: Some("Feat: part 1".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );
        branches.insert(
            "feature-2".to_string(),
            StackBranch {
                name: "feature-2".to_string(),
                parent: Some("feature-1".to_string()),
                children: vec![],
                commit_hash: "hash2".to_string(),
                mr_iid: Some(102),
                mr_title: Some("Feat: part 2".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );

        let stack = Stack {
            id: "stack-1".to_string(),
            name: "test-stack".to_string(),
            base_branch: "main".to_string(),
            branches,
            current_branch: Some("feature-2".to_string()),
            gitlab_project: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut mrs = HashMap::new();
        mrs.insert(
            101,
            MergeRequest {
                id: 1,
                iid: 101,
                title: "Feat: part 1".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-1".to_string(),
                target_branch: "main".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/101".to_string(),
            },
        );
        mrs.insert(
            102,
            MergeRequest {
                id: 2,
                iid: 102,
                title: "Feat: part 2".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-2".to_string(),
                target_branch: "feature-1".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/102".to_string(),
            },
        );

        (stack, mrs)
    }

    fn create_complex_test_stack_and_mrs() -> (Stack, HashMap<u64, MergeRequest>) {
        let mut branches = HashMap::new();
        // Create a more complex stack with multiple branches
        branches.insert(
            "feature-1".to_string(),
            StackBranch {
                name: "feature-1".to_string(),
                parent: Some("main".to_string()),
                children: vec!["feature-2".to_string(), "feature-3".to_string()],
                commit_hash: "hash1".to_string(),
                mr_iid: Some(101),
                mr_title: Some("Feat: part 1".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );
        branches.insert(
            "feature-2".to_string(),
            StackBranch {
                name: "feature-2".to_string(),
                parent: Some("feature-1".to_string()),
                children: vec![],
                commit_hash: "hash2".to_string(),
                mr_iid: Some(102),
                mr_title: Some("Feat: part 2".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );
        branches.insert(
            "feature-3".to_string(),
            StackBranch {
                name: "feature-3".to_string(),
                parent: Some("feature-1".to_string()),
                children: vec![],
                commit_hash: "hash3".to_string(),
                mr_iid: Some(103),
                mr_title: Some("Feat: part 3".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );
        branches.insert(
            "feature-4".to_string(),
            StackBranch {
                name: "feature-4".to_string(),
                parent: Some("main".to_string()),
                children: vec![],
                commit_hash: "hash4".to_string(),
                mr_iid: Some(104),
                mr_title: Some("Feat: part 4".to_string()),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        );

        let stack = Stack {
            id: "stack-1".to_string(),
            name: "test-stack".to_string(),
            base_branch: "main".to_string(),
            branches,
            current_branch: Some("feature-2".to_string()),
            gitlab_project: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut mrs = HashMap::new();
        mrs.insert(
            101,
            MergeRequest {
                id: 1,
                iid: 101,
                title: "Feat: part 1".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-1".to_string(),
                target_branch: "main".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/101".to_string(),
            },
        );
        mrs.insert(
            102,
            MergeRequest {
                id: 2,
                iid: 102,
                title: "Feat: part 2".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-2".to_string(),
                target_branch: "feature-1".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/102".to_string(),
            },
        );
        mrs.insert(
            103,
            MergeRequest {
                id: 3,
                iid: 103,
                title: "Feat: part 3".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-3".to_string(),
                target_branch: "feature-1".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/103".to_string(),
            },
        );
        mrs.insert(
            104,
            MergeRequest {
                id: 4,
                iid: 104,
                title: "Feat: part 4".to_string(),
                description: Some("".to_string()),
                source_branch: "feature-4".to_string(),
                target_branch: "main".to_string(),
                state: "opened".to_string(),
                web_url: "https://gitlab.com/test/repo/-/merge_requests/104".to_string(),
            },
        );

        (stack, mrs)
    }

    #[test]
    fn test_build_stack_table() {
        let (stack, mrs) = create_test_stack_and_mrs();
        let table = build_stack_table(&stack, &mrs);

        assert!(table.contains(STACK_TABLE_START));
        assert!(table.contains(STACK_TABLE_END));
        assert!(table.contains("| #1 | `feature-1` | [Feat: part 1](https://gitlab.com/test/repo/-/merge_requests/101+) |"));
        assert!(table.contains("| #2 | `feature-2` | [Feat: part 2](https://gitlab.com/test/repo/-/merge_requests/102+) |"));
    }

    #[test]
    fn test_build_stack_table_includes_all_branches() {
        let (stack, mrs) = create_complex_test_stack_and_mrs();
        let table = build_stack_table(&stack, &mrs);

        assert!(table.contains(STACK_TABLE_START));
        assert!(table.contains(STACK_TABLE_END));

        // Verify all branches are included in the table
        assert!(
            table.contains("feature-1"),
            "Missing feature-1 in table: {}",
            table
        );
        assert!(
            table.contains("feature-2"),
            "Missing feature-2 in table: {}",
            table
        );
        assert!(
            table.contains("feature-3"),
            "Missing feature-3 in table: {}",
            table
        );
        assert!(
            table.contains("feature-4"),
            "Missing feature-4 in table: {}",
            table
        );

        // Verify all MR links are included
        assert!(
            table.contains("[Feat: part 1](https://gitlab.com/test/repo/-/merge_requests/101+)")
        );
        assert!(
            table.contains("[Feat: part 2](https://gitlab.com/test/repo/-/merge_requests/102+)")
        );
        assert!(
            table.contains("[Feat: part 3](https://gitlab.com/test/repo/-/merge_requests/103+)")
        );
        assert!(
            table.contains("[Feat: part 4](https://gitlab.com/test/repo/-/merge_requests/104+)")
        );

        // Count the number of table rows (should be 4 branches + header rows)
        let table_rows = table
            .lines()
            .filter(|line| line.starts_with("|") && line.contains("#"))
            .count();
        assert_eq!(
            table_rows, 4,
            "Expected 4 branches in table, found {}: {}",
            table_rows, table
        );
    }

    #[test]
    fn test_update_description_no_existing_table() {
        let description = Some("Initial description.".to_string());
        let (stack, mrs) = create_test_stack_and_mrs();
        let new_table = build_stack_table(&stack, &mrs);

        let updated_description = update_description(&description, &new_table);

        assert!(updated_description.starts_with("Initial description."));
        assert!(updated_description.contains(STACK_TABLE_START));
    }

    #[test]
    fn test_update_description_with_existing_table() {
        let old_table =
            "<!-- git-train-stack-start -->\nOld table content\n<!-- git-train-stack-end -->";
        let description = Some(format!(
            "Some text before.\n\n{}\n\nSome text after.",
            old_table
        ));

        let (stack, mrs) = create_test_stack_and_mrs();
        let new_table = build_stack_table(&stack, &mrs);

        let updated_description = update_description(&description, &new_table);

        assert!(updated_description.contains("Some text before."));
        assert!(updated_description.contains("Some text after."));
        assert!(!updated_description.contains("Old table content"));
        assert!(updated_description
            .contains("[Feat: part 2](https://gitlab.com/test/repo/-/merge_requests/102+)"));
    }

    #[test]
    fn test_update_description_empty_description() {
        let description = None;
        let (stack, mrs) = create_test_stack_and_mrs();
        let new_table = build_stack_table(&stack, &mrs);
        let updated_description = update_description(&description, &new_table);

        assert!(updated_description.contains(STACK_TABLE_START));
        assert!(!updated_description.starts_with("\n\n"));
    }
}
