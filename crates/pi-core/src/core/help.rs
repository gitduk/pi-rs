use std::sync::LazyLock;

pub(super) const README: &str = include_str!("../../../../README.md");

static FILES: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| sections(README));

const INDEX: &str = "---\nname: pi-help\n\
description: Read pi's documentation for configuration, tools, sessions, and limits. \
Use this skill when asked how to use pi or how a pi feature works.\n---\n\n\
# pi help\n\n\
The files below come from pi's README.\n\
Read the section that covers the question before answering.\n\
Use `skill` with `name: \"pi-help\"` and `file` set to a listed path.\n\n\
For command-line flags and capability tiers, read `run.md` first.\n\
For model connections and settings, read `configure.md` first.\n\
Read one section first.\n\n\
Do not read another section if the first section answers the question.\n\
Read another section only to resolve a specific missing fact.\n\
Read the parent section only when its context changes the answer.\n\
Read `README.md` only when the task needs the complete documentation.\n";

pub(super) fn skill() -> skills::Skill {
    let mut files = FILES.clone();
    files.push(("README.md".to_string(), README));
    skills::Skill::builtin(INDEX)
        .expect("a valid built-in skill")
        .with_files(files)
}

fn sections(text: &'static str) -> Vec<(String, &'static str)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    let mut fence = None;
    let mut parent = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed
            .as_bytes()
            .first()
            .copied()
            .filter(|c| *c == b'`' || *c == b'~');
        let count = marker.map_or(0, |c| trimmed.bytes().take_while(|b| *b == c).count());
        if let Some((character, length)) = fence {
            if marker == Some(character) && count >= length && trimmed[count..].trim().is_empty() {
                fence = None;
            }
        } else if let Some(character) = marker.filter(|_| count >= 3) {
            fence = Some((character, count));
        } else {
            let heading = line
                .strip_prefix("### ")
                .map(|s| (3, s))
                .or_else(|| line.strip_prefix("## ").map(|s| (2, s)))
                .or_else(|| line.strip_prefix("# ").map(|s| (1, s)));
            if let Some((level, title)) = heading {
                let slug = title
                    .trim()
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|s| !s.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>()
                    .join("-");
                let name = match level {
                    1 => "overview.md".to_string(),
                    2 => {
                        parent = slug.clone();
                        format!("{slug}.md")
                    }
                    _ => format!("{parent}/{slug}.md"),
                };
                starts.push((name, offset));
            }
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, (name, start))| {
            let end = starts.get(i + 1).map_or(text.len(), |(_, start)| *start);
            (name.clone(), &text[*start..end])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tool::Tool;

    #[tokio::test]
    async fn help_loads_an_index_then_the_exact_readme_section() {
        let skill = super::skill();
        let loader = skills::Load::new(vec![skill]);
        let dir = tempfile::tempdir().unwrap();
        let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let initial = loader
            .execute(serde_json::json!({"name": "pi-help"}), &ctx)
            .await
            .unwrap();
        let text = match &initial.content[0] {
            llm::message::ToolResultContent::Text(t) => &t.text,
            _ => panic!("the index must be text"),
        };
        assert!(text.contains("run.md"));
        assert!(!text.contains("`--tier` caps what a run may reach"));
        let run = loader
            .execute(
                serde_json::json!({"name": "pi-help", "file": "run.md"}),
                &ctx,
            )
            .await
            .unwrap();
        let expected = super::README
            .split("## Run\n")
            .nth(1)
            .unwrap()
            .split("## In a session\n")
            .next()
            .unwrap();
        assert!(
            matches!(&run.content[..], [llm::message::ToolResultContent::Text(t)] if t.text == format!("## Run\n{expected}"))
        );
        let full = loader
            .execute(
                serde_json::json!({"name": "pi-help", "file": "README.md"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(&full.content[..], [llm::message::ToolResultContent::Text(t)] if t.text == super::README)
        );
        assert!(
            loader
                .execute(
                    serde_json::json!({"name": "pi-help", "file": "../settings.toml"}),
                    &ctx,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_project_help_skill_still_replaces_the_built_in_documents() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".pi/skills/pi-help");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(
            local.join("SKILL.md"),
            "---\nname: pi-help\ndescription: Local help.\n---\nUse the local instructions.\n",
        )
        .unwrap();
        let workspace = tool::Workspace::new(dir.path()).unwrap();
        let pinned = crate::args::Pinned {
            no_context_files: true,
            tier: Some(pi_store::args::TierArg::Read),
            ..Default::default()
        };
        let settings = pi_store::settings::Settings::new(toml::Table::new().into(), None);
        let resolved = crate::core::resolve::resolve(
            &pinned,
            &workspace,
            &Default::default(),
            &settings,
            None,
            &[],
        )
        .unwrap();
        let loader = resolved.brief.registry.get(skills::Load::NAME).unwrap();
        let ctx = tool::Ctx::new(workspace);
        let out = loader
            .execute(serde_json::json!({"name": "pi-help"}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(&out.content[..], [llm::message::ToolResultContent::Text(t)] if t.text.contains("Use the local instructions.") && !t.text.contains("run.md"))
        );
        assert!(
            loader
                .execute(
                    serde_json::json!({"name": "pi-help", "file": "run.md"}),
                    &ctx,
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn section_files_preserve_the_readme_without_splitting_code_examples() {
        let source = "# pi\nOverview.\n## Tools\nIntro.\n````md\n```rs\n## Not a section\n```\n````\n~~~md\n## Also not a section\n~~~\n### Hooks\nEvents.\n";
        let files = super::sections(source);
        assert_eq!(
            files
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["overview.md", "tools.md", "tools/hooks.md"]
        );
        assert_eq!(
            files.iter().map(|(_, text)| *text).collect::<String>(),
            source
        );
        assert_eq!(
            super::FILES
                .iter()
                .map(|(_, text)| *text)
                .collect::<String>(),
            super::README
        );
        let mut names = super::FILES
            .iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        names.sort();
        assert!(!names.windows(2).any(|w| w[0] == w[1]));
    }
}
