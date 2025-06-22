use crate::stack::types::Stack;
use crate::gitlab::api::MergeRequest;
use std::collections::HashMap;

const STACK_TABLE_START: &str = "<!-- git-train-stack-start -->";
const STACK_TABLE_END: &str = "<!-- git-train-stack-end -->";

pub fn build_stack_table(stack: &Stack, mrs: &HashMap<u64, MergeRequest>) -> String {
    let mut table = String::new();

    table.push_str(STACK_TABLE_START);
    table.push_str("\n\n");
    table.push_str("### Train Stack\n\n");
    table.push_str("| Position | Branch | Merge Request |\n");
    table.push_str("|---|---|---|\n");

    // Find the head of the stack (first branch after base)
    let head_branch_name = stack.branches.values().find(|b| b.parent.as_deref() == Some(&stack.base_branch)).map(|b| b.name.clone());

    let mut branches_in_order = vec![];
    if let Some(head) = head_branch_name {
        let mut current_branch_name = head.clone();
        while let Some(current_branch) = stack.branches.get(&current_branch_name) {
            branches_in_order.push(current_branch.clone());
            if let Some(child) = current_branch.children.first() {
                current_branch_name = child.clone();
            } else {
                break;
            }
        }
    }


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

    table.push_str("\n");
    table.push_str(STACK_TABLE_END);

    table
}

pub fn update_description(current_description: &Option<String>, new_table: &str) -> String {
    let current_desc = current_description.as_deref().unwrap_or("").trim();

    if let (Some(start_idx), Some(end_idx)) = (current_desc.find(STACK_TABLE_START), current_desc.find(STACK_TABLE_END)) {
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
    use crate::stack::types::{Stack, StackBranch};
    use crate::gitlab::api::MergeRequest;
    use chrono::Utc;

    fn create_test_stack_and_mrs() -> (Stack, HashMap<u64, MergeRequest>) {
        let mut branches = HashMap::new();
        branches.insert("feature-1".to_string(), StackBranch {
            name: "feature-1".to_string(),
            parent: Some("main".to_string()),
            children: vec!["feature-2".to_string()],
            commit_hash: "hash1".to_string(),
            mr_iid: Some(101),
            mr_title: Some("Feat: part 1".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        branches.insert("feature-2".to_string(), StackBranch {
            name: "feature-2".to_string(),
            parent: Some("feature-1".to_string()),
            children: vec![],
            commit_hash: "hash2".to_string(),
            mr_iid: Some(102),
            mr_title: Some("Feat: part 2".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

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
        mrs.insert(101, MergeRequest {
            id: 1,
            iid: 101,
            title: "Feat: part 1".to_string(),
            description: Some("".to_string()),
            source_branch: "feature-1".to_string(),
            target_branch: "main".to_string(),
            state: "opened".to_string(),
            web_url: "https://gitlab.com/test/repo/-/merge_requests/101".to_string(),
        });
        mrs.insert(102, MergeRequest {
            id: 2,
            iid: 102,
            title: "Feat: part 2".to_string(),
            description: Some("".to_string()),
            source_branch: "feature-2".to_string(),
            target_branch: "feature-1".to_string(),
            state: "opened".to_string(),
            web_url: "https://gitlab.com/test/repo/-/merge_requests/102".to_string(),
        });

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
        let old_table = "<!-- git-train-stack-start -->\nOld table content\n<!-- git-train-stack-end -->";
        let description = Some(format!("Some text before.\n\n{}\n\nSome text after.", old_table));
        
        let (stack, mrs) = create_test_stack_and_mrs();
        let new_table = build_stack_table(&stack, &mrs);

        let updated_description = update_description(&description, &new_table);

        assert!(updated_description.contains("Some text before."));
        assert!(updated_description.contains("Some text after."));
        assert!(!updated_description.contains("Old table content"));
        assert!(updated_description.contains("[Feat: part 2](https://gitlab.com/test/repo/-/merge_requests/102+)"));
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