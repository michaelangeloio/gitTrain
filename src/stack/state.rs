use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tracing::info;

use crate::errors::TrainError;
use crate::stack::types::Stack;

pub struct StackState {
    train_dir: PathBuf,
}

impl StackState {
    pub fn new(train_dir: PathBuf) -> Result<Self> {
        if !train_dir.exists() {
            fs::create_dir_all(&train_dir)?;
        }
        Ok(Self { train_dir })
    }

    pub fn save_stack(&self, stack: &Stack) -> Result<()> {
        let stack_file = self.train_dir.join(format!("{}.json", stack.id));
        let stack_json = serde_json::to_string_pretty(stack)?;

        fs::write(&stack_file, stack_json)?;

        // Also save a "current" file for easy access
        self.set_current(stack)?;

        info!("Saved stack state to: {:?}", stack_file);
        Ok(())
    }

    pub fn load_current(&self) -> Result<Stack> {
        let current_file = self.train_dir.join("current.json");
        if !current_file.exists() {
            return Err(TrainError::StackError {
                message: "No current stack found. Use `git-train list` to see available stacks and `git-train switch` to activate one.".to_string(),
            }
            .into());
        }

        let stack_id = fs::read_to_string(&current_file)?;
        let stack_file = self.train_dir.join(format!("{}.json", stack_id.trim()));

        if !stack_file.exists() {
            // The current.json file might be stale, pointing to a deleted stack
            return Err(TrainError::StackError {
                message: format!(
                    "Stack file not found for current stack ID '{}'. It may have been deleted. Use `git-train list` and `git-train switch`.",
                    stack_id.trim()
                ),
            }
            .into());
        }

        let stack_json = fs::read_to_string(&stack_file)?;
        let stack: Stack = serde_json::from_str(&stack_json)?;

        Ok(stack)
    }

    pub fn find_by_identifier(&self, stack_identifier: &str) -> Result<Stack> {
        let stacks = self.list()?;
        for stack in stacks {
            if stack.name == stack_identifier || stack.id.starts_with(stack_identifier) {
                return Ok(stack);
            }
        }

        Err(TrainError::StackError {
            message: format!("Stack '{}' not found", stack_identifier),
        }
        .into())
    }

    pub fn list(&self) -> Result<Vec<Stack>> {
        let mut stacks = Vec::new();
        for entry in fs::read_dir(&self.train_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && path.file_stem().is_some_and(|s| s != "current")
            {
                if let Ok(stack_json) = fs::read_to_string(&path) {
                    if let Ok(stack) = serde_json::from_str::<Stack>(&stack_json) {
                        stacks.push(stack);
                    }
                }
            }
        }
        // Sort stacks by name for consistent ordering
        stacks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(stacks)
    }

    pub fn set_current(&self, stack: &Stack) -> Result<()> {
        let current_file = self.train_dir.join("current.json");
        fs::write(&current_file, &stack.id)?;
        Ok(())
    }

    pub fn delete(&self, stack: &Stack) -> Result<()> {
        let stack_file = self.train_dir.join(format!("{}.json", stack.id));
        if stack_file.exists() {
            fs::remove_file(&stack_file)?;
        }

        // If this was the current stack, remove the current pointer
        if let Ok(current_id) = self.get_current_stack_id() {
            if current_id == stack.id {
                let current_file = self.train_dir.join("current.json");
                if current_file.exists() {
                    fs::remove_file(current_file)?;
                }
            }
        }

        Ok(())
    }

    pub fn get_current_stack_id(&self) -> Result<String> {
        let current_file = self.train_dir.join("current.json");
        Ok(fs::read_to_string(current_file).unwrap_or_default())
    }
}
