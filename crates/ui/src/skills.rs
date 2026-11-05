//! Built-in skills supplied by the terminal surface.

/// Instructions and examples for customizing the surface.
pub fn builtins() -> Vec<skills::Skill> {
    vec![
        skills::Skill::builtin(include_str!("../skills/bar-setup/SKILL.md"))
            .expect("a valid built-in skill")
            .with_files(vec![(
                "bar.rs".to_string(),
                include_str!("../../../examples/bar.rs"),
            )]),
    ]
}

#[cfg(test)]
mod tests {
    use tool::Tool;

    #[tokio::test]
    async fn bar_setup_serves_its_instructions_and_example() {
        let loader = skills::Load::new(super::builtins());
        let dir = tempfile::tempdir().unwrap();
        let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let instructions = loader
            .execute(serde_json::json!({"name": "bar-setup"}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(&instructions.content[..], [llm::message::ToolResultContent::Text(t)]
                if t.text.contains("# Customize the bar")
                    && t.text.contains("Files in this skill:")
                    && t.text.contains("- `bar.rs`"))
        );

        let example = loader
            .execute(
                serde_json::json!({"name": "bar-setup", "file": "bar.rs"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(&example.content[..], [llm::message::ToolResultContent::Text(t)]
                if t.text == include_str!("../../../examples/bar.rs"))
        );
    }
}
