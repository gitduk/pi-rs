use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_core::driver::jobs::{Job, State};
use pi_store::keys::Mode;

use super::harness::*;
use crate::tui::{Asked, Deed, TermEvent, View};

fn jobs(ids: &[u64]) -> Vec<Job> {
    ids.iter()
        .map(|&id| Job {
            id,
            description: format!("command {id}"),
            progress: String::new(),
            started: tokio::time::Instant::now(),
            started_at: std::time::SystemTime::now(),
            state: State::Running,
            last: String::new(),
        })
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

#[test]
fn normal_keys_move_from_the_draft_through_jobs_and_back() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&[6, 26, 30]));
    ui.vim = Some(Mode::Normal);
    ui.editor.set_line("the draft");
    ui.editor.left();
    let caret = ui.editor.view(&ui.paint, 80).1;

    ui.key(&lane, &mut view, ctrl('k'), true);
    assert_eq!(ui.job_focus, Some(30));
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('x'), true)),
        Deed::StopJobs(vec![30])
    );
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('r'), true)),
        Deed::RestartJobs(vec![30])
    );
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('X'), true)),
        Deed::StopJobs(Vec::new())
    );
    assert!(matches!(
        ui.key(&lane, &mut view, key(KeyCode::Enter), true),
        Asked::Own(Deed::Job(30))
    ));
    for expected in [26, 6, 6] {
        ui.key(&lane, &mut view, ctrl('k'), true);
        assert_eq!(ui.job_focus, Some(expected));
    }
    for expected in [Some(26), Some(30), None] {
        ui.key(&lane, &mut view, ctrl('j'), true);
        assert_eq!(ui.job_focus, expected);
    }
    ui.key(&lane, &mut view, ctrl('k'), true);
    assert!(
        matches!(
            ui.key(&lane, &mut view, key(KeyCode::Esc), true),
            Asked::Own(Deed::Nothing)
        ),
        "esc returns focus without interrupting the run"
    );
    assert_eq!(ui.job_focus, None);
    assert_eq!(ui.editor.text(), "the draft");
    assert_eq!(ui.editor.view(&ui.paint, 80).1, caret);
    assert_eq!(ui.vim, Some(Mode::Normal));
}

#[test]
fn focus_tracks_ids_and_keeps_stopped_rows_available_for_restart() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&[6, 26, 30]));
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), false);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.job_focus, Some(26));
    let mut changed = jobs(&[26, 30, 99]);
    changed[0].state = State::Stopped;
    ui.sync_jobs(changed.clone());
    assert_eq!(
        ui.job_focus,
        Some(26),
        "another job disappearing must not change the target"
    );
    assert_eq!(ui.jobs[0].state, State::Stopped);
    assert_eq!(
        deed(ui.key(&lane, &mut view, typed('r'), false)),
        Deed::RestartJobs(vec![26])
    );

    ui.key(&lane, &mut view, key(KeyCode::Esc), false);
    assert!(ui.jobs.iter().all(|j| j.state == State::Running));
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.job_focus, Some(99));
    ui.sync_jobs(jobs(&[30]));
    assert_eq!(
        ui.job_focus,
        Some(30),
        "a removed target moves to its nearest surviving row"
    );
    ui.sync_jobs(Vec::new());
    assert_eq!(ui.job_focus, None);
}

#[test]
fn insert_mode_menus_and_pages_keep_their_keys() {
    use pi_core::core::question::Question;
    use toolbox::background::Asked as QuestionText;

    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&[6]));
    ui.editor.set_line("abcdef");
    ui.editor.left();
    ui.editor.left();
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.editor.text(), "abcd");
    assert_eq!(ui.job_focus, None);
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
    assert_eq!(ui.job_focus, None, "question options take precedence");
    assert!(ui.list.picked.is_some());
    ui.sync_questions(Vec::new());
    ui.open_page(vec![ratatui::text::Line::from("page").into()]);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.job_focus, None, "reading owns its keys");
    ui.close_page();
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.job_focus, Some(6));
    ui.set_mode(Mode::Insert);
    assert_eq!(ui.job_focus, None);
}

#[test]
fn the_selected_job_is_visible_and_highlighted_on_a_short_screen() {
    let mut ui = vim_ui_at(80, 9);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&(1..=20).collect::<Vec<_>>()));
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), false);
    ui.flush(&lane, &mut view);
    let row = ui
        .job_rows
        .iter()
        .position(|r| *r == crate::tui::call::JobRow::Job(20))
        .expect("the focused job is drawn past the fold");
    let pos = (ui.regions.jobs.x, ui.regions.jobs.y + row as u16);
    assert_eq!(
        ui.screen.test_cursor(),
        pos.into(),
        "the caret moves to the selected job"
    );
    assert!(
        ui.screen.test_buffer()[pos]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
    assert!(
        ui.drawn.iter().any(|line| line.contains("x stop")),
        "actions stay visible"
    );

    ui.job_open = Some(crate::tui::ui::OpenJob {
        id: 20,
        body: "the job output".into(),
    });
    ui.flush(&lane, &mut view);
    assert!(
        ui.drawn.iter().any(|line| line.contains("the job output")),
        "enter's expanded output stays in view"
    );
    ui.key(&lane, &mut view, key(KeyCode::Esc), false);
    ui.flush(&lane, &mut view);
    assert!(
        ui.screen.test_cursor().y >= ui.regions.editor.y,
        "the caret returns to the draft"
    );
}

#[test]
fn leaving_the_lane_or_disabling_vim_releases_job_focus() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&[1]));
    ui.vim = Some(Mode::Normal);
    ui.key(&lane, &mut view, ctrl('k'), false);
    ui.leave_lane(&mut view);
    assert_eq!(ui.job_focus, None);
    ui.key(&lane, &mut view, ctrl('k'), false);
    assert_eq!(ui.job_focus, Some(1));
    ui.set_vim(&pi_store::config::Vim {
        enabled: false,
        ..Default::default()
    });
    assert_eq!(ui.job_focus, None);
}

#[tokio::test]
async fn focused_controls_use_the_existing_driver_and_preserve_restartability() {
    use std::sync::Arc;
    use toolbox::background::{JobHandle, JobSink};

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let root = tui.core.lane().root().to_path_buf();
    let table = tui.drivers.jobs();
    let sink = pi_core::driver::jobs::Jobs::new(table.clone());
    let stop = tokio_util::sync::CancellationToken::new();
    let first = sink.start(root.clone(), "first".into(), stop.clone(), None);
    let second = sink.start(root.clone(), "second".into(), Default::default(), None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Arc<dyn JobHandle>>();
    second.rerun(
        Arc::new(move |job, _| {
            let _ = tx.send(job);
            Ok(())
        }),
        None,
    );
    tui.ui.sync_jobs(table.jobs(&root));
    tui.ui.vim = Some(Mode::Normal);
    press(&mut tui, ctrl('k'));
    let asked = press(&mut tui, typed('x'));
    tui.carry(deed(asked)).await;
    assert!(
        drawn_rows(crate::tui::view_at(&mut tui.views, tui.core.lane().token()))
            .iter()
            .any(|line| line.contains("The user stopped background job")),
        "stopping is recorded for the model"
    );
    assert!(!stop.is_cancelled(), "only the selected job stopped");
    tui.ui.sync_jobs(table.jobs(&root));
    assert_eq!(tui.ui.job_focus, Some(second.id()));
    assert_eq!(tui.ui.jobs.last().unwrap().state, State::Stopped);
    let asked = press(&mut tui, typed('r'));
    tui.carry(deed(asked)).await;
    assert!(
        drawn_rows(crate::tui::view_at(&mut tui.views, tui.core.lane().token()))
            .iter()
            .any(|line| line.contains("The user restarted background job")),
        "restarting is recorded for the model"
    );
    let id = second.id();
    drop(second);
    let restarted = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restarted.id(), id);
    tui.ui.sync_jobs(table.jobs(&root));
    let asked = press(&mut tui, typed('X'));
    tui.carry(deed(asked)).await;
    assert!(
        stop.is_cancelled(),
        "stop-all includes the other running job"
    );
    assert!(table.jobs(&root).iter().all(|j| j.state != State::Running));
    drop((first, restarted));
}

#[test]
fn global_cancel_keeps_working_and_paste_does_not_change_a_focused_draft() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.sync_jobs(jobs(&[1]));
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
    assert_eq!(ui.job_focus, None);
}

#[test]
fn waiting_results_fold_without_wrapping_long_commands() {
    use crate::tui::mouse::Target;
    use crate::tui::screen::{plain, rows};

    let (_dir, lane) = a_running_lane();
    let view = View::default();
    let mut pending = jobs(&(1..=14).collect::<Vec<_>>());
    for job in &mut pending {
        job.state = State::Back;
        job.description = format!("python3 -c '{}'", "长命令 ".repeat(100));
    }
    pending.last_mut().unwrap().last = "KeyError: 'agentId'".into();
    pending.extend(jobs(&[15]));

    for width in [20, 40, 80] {
        let mut ui = test_ui(width, 24);
        ui.sync_jobs(pending.clone());
        let (live, targets) = ui.live(&lane, &view, false, std::time::Instant::now());
        assert_eq!(live.len(), targets.len());
        let at = targets
            .iter()
            .position(|t| matches!(t, Target::JobResults(_)))
            .unwrap();
        assert_eq!(live.len(), at + 1, "all results share one row");
        assert_eq!(rows(&live[at], width as usize), 1);
        if width == 80 {
            assert!(plain(&live[at]).contains(&pending[13].last));
        }

        ui.job_results_shown = true;
        let (live, targets) = ui.live(&lane, &view, false, std::time::Instant::now());
        let ids: Vec<_> = targets
            .iter()
            .filter_map(|t| match t {
                Target::JobResult(_, id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, (1..=14).collect::<Vec<_>>());
        assert!(live[at..].iter().all(|l| rows(l, width as usize) == 1));
        assert_eq!(ui.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [15]);

        ui.sync_jobs(jobs(&[15]));
        assert!(!ui.job_results_shown, "the next batch starts folded");
    }
}

#[tokio::test]
async fn waiting_results_expand_and_open_the_original_output_without_delivering_it() {
    use crate::tui::mouse::Target;
    use crate::tui::screen::plain;

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let root = tui.core.lane().root().to_path_buf();
    let table = tui.drivers.jobs();
    let sink = pi_core::driver::jobs::Jobs::new(table.clone());
    let command = format!("python3 -c '{}'", "long command ".repeat(30));
    let job = toolbox::background::JobSink::start(
        &sink,
        root.clone(),
        command.clone(),
        Default::default(),
        None,
    );
    let output = "exited 1:\nTraceback (most recent call last):\nKeyError: 'agentId'";
    job.result(output.into());
    job.end();
    tui.ui.sync_jobs(table.jobs(&root));

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
        .position(|t| matches!(t, Target::JobResults(_)))
        .unwrap();
    let asked = click(&mut tui, at);
    assert_eq!(deed(asked), Deed::JobResults(true));
    tui.carry(Deed::JobResults(true)).await;
    flush(&mut tui);
    let at = tui
        .ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::JobResult(_, id) if *id == job.id()))
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
    assert_eq!(table.waiting(&root, job.id()).unwrap(), [output]);

    tui.ui.close_page();
    flush(&mut tui);
    let at = tui
        .ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::JobResults(_)))
        .unwrap();
    let asked = click(&mut tui, at);
    tui.carry(deed(asked)).await;
    assert!(!tui.ui.job_results_shown);
}
