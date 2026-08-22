use serde_json::json;
use tools::skills::{Skill, discover_from};
use tools::{Ctx, Tool, ToolError, Workspace};

fn tree() -> (tempfile::TempDir, Vec<Skill>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("skills");

    let thinking = root.join("thinking");
    std::fs::create_dir_all(thinking.join("references")).unwrap();
    std::fs::write(
        thinking.join("SKILL.md"),
        "---\nname: thinking\ndescription: \"Five models, one router.\"\n---\n\n# Thinking\n\nPick one, read its file.\n",
    )
    .unwrap();
    std::fs::write(
        thinking.join("references/inversion.md"),
        "# Inversion\n\nWork backwards.\n",
    )
    .unwrap();

    let commit = root.join("commit");
    std::fs::create_dir_all(&commit).unwrap();
    std::fs::write(
        commit.join("SKILL.md"),
        "---\ndescription: Write a commit.\n---\n\nStage, then commit.\n",
    )
    .unwrap();

    let found = discover_from(&[root]);
    (dir, found.skills)
}

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

#[test]
fn descriptions_ride_in_the_tool_block_and_bodies_do_not() {
    let (_d, found) = tree();
    let tool = tools::skill::SkillTool::new(found);
    let d = tool.description();

    assert!(d.contains("- thinking: Five models, one router."), "{d}");
    assert!(d.contains("- commit: Write a commit."), "{d}");
    // The split is the point: a dozen skills cost a paragraph, not their bodies.
    assert!(!d.contains("Pick one, read its file"), "{d}");
}

#[tokio::test]
async fn loading_a_skill_returns_its_body_without_the_header() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    let out = tools::skill::SkillTool::new(found)
        .execute(json!({ "name": "thinking" }), &c)
        .await
        .unwrap()
        .flatten();

    assert!(out.starts_with("# Thinking"), "{out}");
    assert!(
        !out.contains("description:"),
        "the header is not instructions: {out}"
    );
}

#[tokio::test]
async fn a_skill_can_hand_over_its_own_reference_files() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    let out = tools::skill::SkillTool::new(found)
        .execute(
            json!({ "name": "thinking", "file": "references/inversion.md" }),
            &c,
        )
        .await
        .unwrap()
        .flatten();
    assert!(out.contains("Work backwards"), "{out}");
}

#[tokio::test]
async fn a_file_argument_cannot_leave_the_skill_directory() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    // Skills live outside the workspace, so its gate does not cover them.
    let r = tools::skill::SkillTool::new(found)
        .execute(
            json!({ "name": "thinking", "file": "../commit/SKILL.md" }),
            &c,
        )
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
}

#[tokio::test]
async fn an_unknown_name_lists_what_there_is() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    let err = tools::skill::SkillTool::new(found)
        .execute(json!({ "name": "nope" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no skill named `nope`"), "{err}");
    assert!(err.contains("commit, thinking"), "{err}");
}

#[test]
fn a_directory_with_no_skill_file_is_not_a_skill() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("skills/notaskill")).unwrap();
    std::fs::write(dir.path().join("skills/notaskill/README.md"), "hi").unwrap();
    assert!(
        discover_from(&[dir.path().join("skills")])
            .skills
            .is_empty()
    );
}

#[tokio::test]
async fn a_loaded_skill_says_how_to_reach_its_own_files() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    let out = tools::skill::SkillTool::new(found)
        .execute(json!({ "name": "thinking" }), &c)
        .await
        .unwrap()
        .flatten();

    // Skills usually sit outside the workspace, where `read` cannot reach.
    assert!(out.contains("references/inversion.md"), "{out}");
    assert!(
        out.contains(r#"skill(name: "thinking", file: "<path>")"#),
        "{out}"
    );
    assert!(out.contains("`read` cannot reach them"), "{out}");
}

#[tokio::test]
async fn a_skill_with_no_extra_files_gets_no_footer() {
    let (_d, found) = tree();
    let (_w, c) = ctx();
    let out = tools::skill::SkillTool::new(found)
        .execute(json!({ "name": "commit" }), &c)
        .await
        .unwrap()
        .flatten();
    assert!(!out.contains("Files in this skill"), "{out}");
    assert!(out.contains("Stage, then commit"), "{out}");
}
