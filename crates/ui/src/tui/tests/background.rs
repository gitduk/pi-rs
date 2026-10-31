use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_core::driver::background::{BackgroundCommand, Invocation, State};
use pi_store::keys::Mode;

use super::harness::*;
use crate::tui::{Asked, Deed, TermEvent, View};

// What the lane is handed once the checkout's command has ended. Polled,
// since nothing in a run waits on one command to finish.
async fn said_by(table: &pi_core::driver::background::Table, root: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(due) = table.take_ready_results(root) {
            return due.line;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the command said nothing in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn background(ids: &[u64]) -> Vec<BackgroundCommand> {
    ids.iter()
        .map(|&id| background_command(id, Invocation::shell(format!("command {id}"))))
        .collect()
}

fn key(code: KeyCode) -> TermEvent {
    TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn deed(asked: Asked) -> Deed {
    match asked {
        Asked::Own(deed) => deed,
        other => panic!("expected a surface action, got {other:?}"),
    }
}

fn press(tui: &mut crate::tui::Tui, key: TermEvent) -> Asked {
    let token = tui.core.lane().token();
    tui.ui.key(
        tui.core.lane(),
        crate::tui::view_at(&mut tui.views, token),
        key,
        true,
    )
}

#[cfg(unix)]
#[tokio::test]
async fn named_scripts_use_bash_background_with_original_input_and_environment() {
    use std::sync::Arc;
    use tool::Source;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("later"),
        "#!/bin/sh\n# ---\n# description = \"Keep a prompt\"\n# background = true\n# [optional]\n\
         # after = \"A delay\"\n# prompt = \"What to do\"\n# ---\n\
         cat > received\nprintf '%s\\n' '{\"status\":\"waiting\"}' >&3\nsleep 30\n",
    )
    .unwrap();
    let table = Arc::new(pi_core::driver::background::Table::default());
    let sink = Arc::new(pi_core::driver::background::Commands::new(table.clone()));
    let shell = Arc::new(toolbox::bash::Bash::new(sink.clone()));
    let scripts = toolbox::scripts::Dir::new(dir.path()).with_shell(shell, sink);
    let script = scripts.tools().pop().unwrap();
    let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
    let mut ui = test_ui(160, 24);
    let lane = running_lane(dir.path());
    let mut view = View::default();

    for args in [
        serde_json::json!({ "after": "30m", "prompt": "check build\nreport status" }),
        serde_json::json!({}),
    ] {
        script.execute(args.clone(), &ctx).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if table
                    .background(dir.path())
                    .last()
                    .is_some_and(|command| command.progress == "waiting")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the script received its input");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(dir.path().join("received")).unwrap()
            )
            .unwrap(),
            args,
        );
        let executions = table.background(dir.path());
        let execution = executions.last().unwrap();
        assert!(
            matches!(execution.invocation.as_ref(), Invocation::Tool { name, args: actual } if name == "later" && actual == &args)
        );
        let carried = table.carried(dir.path());
        assert_eq!(
            carried.last().unwrap().again.stdin.as_deref(),
            Some(args.to_string().as_str())
        );
        let head = format!("#{} {}", execution.id, execution.invocation.title());
        ui.sync_background(executions);
        ui.flush(&lane, &mut view);
        let buffer = ui.screen.test_buffer();
        let area = ui.regions.executions;
        let rows: Vec<String> = (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(rows.iter().any(|row| row.starts_with(&head)), "{rows:?}");
    }
    table.stop_running(dir.path(), "test finished");
}

#[cfg(unix)]
#[tokio::test]
async fn script_restarts_reload_private_environment_and_reuse_original_arguments() {
    use std::sync::Arc;
    use tool::{Source, Tool};

    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools with 'quotes");
    std::fs::create_dir(&tools).unwrap();
    std::fs::write(
        tools.join("show"),
        r#"#!/bin/sh
# ---
# description = "Show test input"
# background = true
# [optional]
# PI_BACKGROUND_TEST_VALUE = "A value"
# ---
cat > received
printf '%s\n' "$PI_BACKGROUND_TEST_VALUE"
printf '%s\n' 'private stderr' >&2
"#,
    )
    .unwrap();
    let env = tools.join(".env");
    std::fs::write(&env, "PI_BACKGROUND_TEST_VALUE=from-file\n").unwrap();
    let table = Arc::new(pi_core::driver::background::Table::default());
    let host = Arc::new(pi_core::driver::background::Commands::new(table.clone()));
    let shell = Arc::new(toolbox::bash::Bash::new(host.clone()).with_controls(host.clone()));
    let scripts = toolbox::scripts::Dir::new(&tools).with_shell(shell.clone(), host);
    let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
    let args = serde_json::json!({"PI_BACKGROUND_TEST_VALUE":"original argument\nwith newline"});
    scripts.tools()[0]
        .execute(args.clone(), &ctx)
        .await
        .unwrap();
    let first = said_by(&table, dir.path()).await;
    assert!(first.contains("from-file"), "{first}");
    assert!(!first.contains("private stderr"));
    assert!(
        table
            .output(dir.path(), 1)
            .unwrap()
            .contains("private stderr")
    );
    let invocation = Invocation::Tool {
        name: "show".into(),
        args: args.clone(),
    };
    let carried = table.carried(dir.path());
    assert_eq!(carried[0].invocation, invocation);
    let saved = serde_json::to_string(&carried).unwrap();
    assert!(
        !saved.contains("from-file"),
        "environment-file values must not be persisted"
    );
    assert_eq!(
        carried[0].again.stdin.as_deref(),
        Some(args.to_string().as_str())
    );

    let table = Arc::new(pi_core::driver::background::Table::default());
    let restored: Vec<pi_core::driver::background::Carried> = serde_json::from_str(&saved).unwrap();
    for carried in restored {
        let rerun = toolbox::bash::rerun(carried.again.clone(), &ctx);
        table.revive(carried, rerun);
    }
    assert_eq!(
        table.background(dir.path())[0].invocation.as_ref(),
        &invocation
    );
    assert!(table.listing(dir.path())[0].contains(invocation.title().as_ref()));
    std::fs::write(&env, "PI_BACKGROUND_TEST_VALUE=updated-file\n").unwrap();
    table.restart(dir.path(), 1).unwrap();
    let second = said_by(&table, dir.path()).await;
    assert!(second.contains("updated-file"), "{second}");
    assert_eq!(
        table.background(dir.path())[0].invocation.as_ref(),
        &invocation
    );

    std::fs::remove_file(&env).unwrap();
    table.restart(dir.path(), 1).unwrap();
    let third = said_by(&table, dir.path()).await;
    assert!(third.contains("original argument\nwith newline"), "{third}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(dir.path().join("received")).unwrap()
        )
        .unwrap(),
        args
    );

    std::fs::write(&env, "PI_BACKGROUND_TEST_VALUE=private-again\n").unwrap();
    let ordinary = shell
        .execute(
            serde_json::json!({"command":"printf '%s' \"${PI_BACKGROUND_TEST_VALUE-unset}\""}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(ordinary.flatten().contains("unset"));
}

#[test]
fn expanded_output_follows_the_heading_without_launch_details() {
    use crate::render::Paint;
    use crate::tui::{call, ui::OpenCommand};
    use pi_core::driver::background::Launch;

    let mut command = background_command(
        1,
        Invocation::Tool {
            name: "bridge".into(),
            args: serde_json::json!({"channel":"some words"}),
        },
    );
    command.launch = Some(Launch {
        command: "cargo +nightly -Zscript /some/path/bridge.rs\necho finished".into(),
        cwd: "/some/workspace".into(),
    });
    let open = OpenCommand {
        id: 1,
        body: (0..20).map(|i| format!("output {i}\n")).collect(),
    };
    for width in [12, 80] {
        let (lines, targets) = call::background_lines(
            &[command.clone()],
            None,
            Some(&open),
            false,
            &Paint::new(false),
            width,
        );
        assert_eq!(lines.len(), targets.len());
        let earlier = targets
            .iter()
            .position(|target| *target == call::CommandRow::Earlier(1))
            .unwrap();
        let last_head = targets
            .iter()
            .rposition(|target| *target == call::CommandRow::BackgroundCommand(1))
            .unwrap();
        assert_eq!(earlier, last_head + 1);
        assert!(
            targets[earlier + 1..]
                .iter()
                .all(|target| *target == call::CommandRow::Under)
        );
    }
    command.last.clear();
    assert!(
        !call::background_opens(&command, 80),
        "launch details alone do not make a command expandable"
    );
}

#[tokio::test]
async fn background_commands_share_shell_colours_and_keep_status_muted() {
    use crate::render::Paint;
    use crate::tui::call;
    use std::sync::Arc;
    use tool::Tool;

    let dir = tempfile::tempdir().unwrap();
    let table = Arc::new(pi_core::driver::background::Table::default());
    let sink = Arc::new(pi_core::driver::background::Commands::new(table.clone()));
    let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
    let command = "python3 .pi/tmp/monitor.py 600 --label 'watch'";
    toolbox::bash::Bash::new(sink.clone())
        .execute(
            serde_json::json!({ "command": command, "background": true }),
            &ctx,
        )
        .await
        .unwrap();
    let mut executions = table.background(dir.path());
    for execution in &mut executions {
        execution.progress = "watching every 60s".into();
    }
    let paint = Paint::new(true);
    for hovered in [false, true] {
        let hover = hovered.then_some(call::CommandRow::BackgroundCommand(executions[0].id));
        let (lines, _) = call::background_lines(&executions, hover, None, false, &paint, 160);
        let expected = paint.shell(command, hovered);
        assert_eq!(&lines[0].spans[1..1 + expected.len()], expected);
        let progress = lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("watching every 60s"))
            .unwrap();
        assert_eq!(
            progress.style.fg,
            paint.span(&paint.theme.muted, "").style.fg
        );
    }
    for width in [24, 160] {
        let (colour, _) = call::background_lines(&executions, None, None, false, &paint, width);
        let (plain, _) =
            call::background_lines(&executions, None, None, false, &Paint::new(false), width);
        for (colour, plain) in colour.iter().zip(plain) {
            assert_eq!(colour.to_string(), plain.to_string());
        }
    }
    table.stop_running(dir.path(), "test finished");
}

#[test]
fn normal_keys_move_from_the_draft_through_commands_and_back() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_background(background(&[6, 26, 30]));
    ui.vim = Some(Mode::Normal);
    ui.editor.set_line("the draft");
    ui.editor.left();
    let caret = ui.editor.view(&ui.paint, 80).1;

    ui.key(&lane, &mut view, ctrl('k'), true);
    assert_eq!(ui.background_focus, Some(30));
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('x'), true)),
        Deed::StopCommands(vec![30])
    );
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('r'), true)),
        Deed::RestartCommands(vec![30])
    );
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('X'), true)),
        Deed::StopCommands(Vec::new())
    );
    assert!(matches!(
        ui.key(&lane, &mut view, key(KeyCode::Enter), true),
        Asked::Own(Deed::CommandOutput(30))
    ));
    for expected in [26, 6, 6] {
        ui.key(&lane, &mut view, ctrl('k'), true);
        assert_eq!(ui.background_focus, Some(expected));
    }
    for expected in [Some(26), Some(30), None] {
        ui.key(&lane, &mut view, ctrl('j'), true);
        assert_eq!(ui.background_focus, expected);
    }
    ui.key(&lane, &mut view, ctrl('k'), true);
    assert!(
        matches!(
            ui.key(&lane, &mut view, key(KeyCode::Esc), true),
            Asked::Own(Deed::Nothing)
        ),
        "esc returns focus without interrupting the run"
    );
    assert_eq!(ui.background_focus, None);
    assert_eq!(ui.editor.text(), "the draft");
    assert_eq!(ui.editor.view(&ui.paint, 80).1, caret);
    assert_eq!(ui.vim, Some(Mode::Normal));
}

#[test]
fn focus_tracks_ids_and_keeps_stopped_rows_available_for_restart() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_background(background(&[6, 26, 30]));
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), false);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.background_focus, Some(26));
    let mut changed = background(&[26, 30, 99]);
    changed[0].state = State::Stopped;
    ui.sync_background(changed.clone());
    assert_eq!(
        ui.background_focus,
        Some(26),
        "another execution disappearing must not change the target"
    );
    assert_eq!(ui.executions[0].state, State::Stopped);
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('r'), false)),
        Deed::RestartCommands(vec![26])
    );

    ui.key(&lane, &mut view, key(KeyCode::Esc), false);
    assert!(ui.executions.iter().all(|j| j.state == State::Running));
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.background_focus, Some(99));
    ui.sync_background(background(&[30]));
    assert_eq!(
        ui.background_focus,
        Some(30),
        "a removed target moves to its nearest surviving row"
    );
    ui.sync_background(Vec::new());
    assert_eq!(ui.background_focus, None);
}

#[test]
fn insert_mode_menus_and_pages_keep_their_keys() {
    use pi_core::core::question::Question;
    use toolbox::channel::Asked as QuestionText;

    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_background(background(&[6]));
    ui.editor.set_line("abcdef");
    ui.editor.left();
    ui.editor.left();
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.editor.text(), "abcd");
    assert_eq!(ui.background_focus, None);
    ui.editor.clear();
    ui.vim = Some(Mode::Normal);
    let mut q = Question::of(
        pi_core::core::question::Asker::Tool {
            name: "test".into(),
        },
        &QuestionText {
            question: "choose".into(),
            options: vec!["one".into(), "two".into()],
            back: false,
        },
    )
    .unwrap();
    q.id = 1;
    ui.sync_questions(vec![q]);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(
        ui.background_focus, None,
        "question options take precedence"
    );
    assert!(ui.list.picked.is_some());
    ui.sync_questions(Vec::new());
    ui.open_page(vec![ratatui::text::Line::from("page").into()]);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.background_focus, None, "reading owns its keys");
    ui.close_page();
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.background_focus, Some(6));
    ui.set_mode(Mode::Insert);
    assert_eq!(ui.background_focus, None);
}

#[test]
fn leaving_the_lane_or_disabling_vim_releases_command_focus() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_background(background(&[1]));
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), false);
    ui.leave_lane(&mut view);
    assert_eq!(ui.background_focus, None);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.background_focus, Some(1));
    ui.set_vim(&pi_store::config::Vim {
        enabled: false,
        ..Default::default()
    });
    assert_eq!(ui.background_focus, None);
}

#[tokio::test]
async fn focused_controls_use_the_existing_driver_and_preserve_restartability() {
    use std::sync::Arc;
    use toolbox::background::{RunHandle, RunSink};

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let root = tui.core.lane().root().to_path_buf();
    let table = tui.drivers.background();
    let sink = pi_core::driver::background::Commands::new(table.clone());
    let stop = tokio_util::sync::CancellationToken::new();
    let first = sink.start(
        root.clone(),
        toolbox::background::Invocation::shell("first"),
        stop.clone(),
    );
    let second = sink.start(
        root.clone(),
        toolbox::background::Invocation::shell("second"),
        Default::default(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Arc<dyn RunHandle>>();
    second.rerun(
        Arc::new(move |execution, _| {
            let _ = tx.send(execution);
            Ok(())
        }),
        Default::default(),
    );
    tui.ui.sync_background(table.background(&root));
    tui.ui.vim = Some(Mode::Normal);
    press(&mut tui, ctrl('k'));
    let asked = press(&mut tui, typed('x'));
    tui.carry(deed(asked)).await;
    assert!(
        drawn_rows(crate::tui::view_at(&mut tui.views, tui.core.lane().token()))
            .iter()
            .any(|line| line.contains("The user stopped background command")),
        "stopping is recorded for the model"
    );
    assert!(!stop.is_cancelled(), "only the selected execution stopped");
    tui.ui.sync_background(table.background(&root));
    assert_eq!(tui.ui.background_focus, Some(second.id()));
    assert_eq!(tui.ui.executions.last().unwrap().state, State::Stopped);
    let asked = press(&mut tui, typed('r'));
    tui.carry(deed(asked)).await;
    assert!(
        drawn_rows(crate::tui::view_at(&mut tui.views, tui.core.lane().token()))
            .iter()
            .any(|line| line.contains("The user restarted background command")),
        "restarting is recorded for the model"
    );
    let id = second.id();
    drop(second);
    let restarted = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restarted.id(), id);
    tui.ui.sync_background(table.background(&root));
    let asked = press(&mut tui, typed('X'));
    tui.carry(deed(asked)).await;
    assert!(
        stop.is_cancelled(),
        "stop-all includes the other running execution"
    );
    assert!(
        table
            .background(&root)
            .iter()
            .all(|j| j.state != State::Running)
    );
    drop((first, restarted));
}

#[test]
fn global_cancel_keeps_working_and_paste_does_not_change_a_focused_draft() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_background(background(&[1]));
    ui.editor.set_line("draft");
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), true);
    ui.key(
        &lane,
        &mut view,
        TermEvent::Paste("unseen text".into()),
        true,
    );
    assert_eq!(ui.editor.text(), "draft");
    assert!(matches!(
        ui.key(&lane, &mut view, ctrl('c'), true),
        Asked::Own(Deed::Interrupt)
    ));
    assert_eq!(ui.background_focus, None);
}

#[test]
fn waiting_results_fold_without_wrapping_long_commands() {
    use crate::tui::mouse::Target;
    use crate::tui::screen::{plain, rows};

    let (_dir, lane) = a_running_lane();
    let view = View::default();
    let mut pending = background(&(1..=14).collect::<Vec<_>>());
    for execution in &mut pending {
        execution.state = State::Back;
        execution.invocation = std::sync::Arc::new(Invocation::shell(format!(
            "python3 -c '{}'",
            "长命令 ".repeat(100)
        )));
    }
    pending.last_mut().unwrap().last = "KeyError: 'agentId'".into();
    pending.extend(background(&[15]));

    for width in [20, 40, 80] {
        let mut ui = test_ui(width, 24);
        ui.sync_background(pending.clone());
        let (live, targets) = ui.live(&lane, &view, false, std::time::Instant::now());
        assert_eq!(live.len(), targets.len());
        let at = targets
            .iter()
            .position(|t| matches!(t, Target::BackgroundResults(_)))
            .unwrap();
        assert_eq!(live.len(), at + 1, "all results share one row");
        assert_eq!(rows(&live[at], width as usize), 1);
        if width == 80 {
            assert!(plain(&live[at]).contains(&pending[13].last));
        }

        ui.background_results_shown = true;
        let (live, targets) = ui.live(&lane, &view, false, std::time::Instant::now());
        let ids: Vec<_> = targets
            .iter()
            .filter_map(|t| match t {
                Target::BackgroundResult(_, id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, (1..=14).collect::<Vec<_>>());
        assert!(live[at..].iter().all(|l| rows(l, width as usize) == 1));
        assert_eq!(ui.executions.iter().map(|j| j.id).collect::<Vec<_>>(), [15]);

        ui.sync_background(background(&[15]));
        assert!(!ui.background_results_shown, "the next batch starts folded");
    }
}

#[tokio::test]
async fn waiting_results_expand_and_open_the_original_output_without_delivering_it() {
    use crate::tui::mouse::Target;
    use crate::tui::screen::plain;

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let root = tui.core.lane().root().to_path_buf();
    let table = tui.drivers.background();
    let sink = pi_core::driver::background::Commands::new(table.clone());
    let command = format!("python3 -c '{}'", "long command ".repeat(30));
    let execution = toolbox::background::RunSink::start(
        &sink,
        root.clone(),
        toolbox::background::Invocation::shell(command.clone()),
        Default::default(),
    );
    let output = "exited 1:\nTraceback (most recent call last):\nKeyError: 'agentId'";
    execution.channel().result(output.into());
    execution.end();
    tui.ui.sync_background(table.background(&root));

    let flush = |tui: &mut crate::tui::Tui| {
        let view = crate::tui::view_at(&mut tui.views, tui.core.lane().token());
        tui.ui.flush(tui.core.lane(), view);
    };
    let click = |tui: &mut crate::tui::Tui, at: usize| {
        press(
            tui,
            TermEvent::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 5,
                row: tui.ui.regions.history.y + at as u16,
                modifiers: KeyModifiers::NONE,
            }),
        )
    };

    flush(&mut tui);
    let at = tui
        .ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::BackgroundResults(_)))
        .unwrap();
    let asked = click(&mut tui, at);
    assert_eq!(deed(asked), Deed::BackgroundResults(true));
    tui.carry(Deed::BackgroundResults(true)).await;
    flush(&mut tui);
    let at = tui
        .ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::BackgroundResult(_, id) if *id == execution.id()))
        .unwrap();
    let asked = click(&mut tui, at);
    tui.carry(deed(asked)).await;
    let page = tui
        .ui
        .page()
        .unwrap()
        .iter()
        .map(|r| plain(&r.line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(page.contains(&command), "the command is whole");
    assert!(page.contains(output), "the complete error is available");
    assert_eq!(table.waiting(&root, execution.id()).unwrap(), [output]);

    tui.ui.close_page();
    flush(&mut tui);
    let at = tui
        .ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::BackgroundResults(_)))
        .unwrap();
    let asked = click(&mut tui, at);
    tui.carry(deed(asked)).await;
    assert!(!tui.ui.background_results_shown);
}
