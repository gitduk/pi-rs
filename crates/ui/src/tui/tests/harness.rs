use crate::render::Paint;
use crate::tui::View;
use crate::tui::row::Row;
use crate::tui::screen::{self, plain};
use crate::tui::scrollback::ScrollbackRows;
use pi_core::core::Core;
use pi_core::core::lane::Lane;
use pi_store::keys::{Keys, Mode};
use pi_store::session::Store;
use ratatui::text::Line;

pub(super) fn drawn_rows(view: &View) -> Vec<String> {
    ScrollbackRows::new(&view.surface.scrollback, &Paint::new(false), 80, |_| true)
        .map(text)
        .collect()
}

pub(super) fn block(id: u64, n: usize, folded: bool) -> Row {
    Row::thinking(
        id,
        (1..=n).map(|i| Line::from(format!("line {i}"))).collect(),
        folded,
    )
}

// The text one line of the view shows: the walk hands over pieces not yet
// built into rows, so this joins them the way a screen would.
pub(super) fn text(piece: crate::tui::scrollback::Piece<'_>) -> String {
    screen::Piece::pieces(piece).iter().map(plain).collect()
}

// One frame of the scrolled-up view, each line known by its index: what
// the window shows, opened on the top line the last frame kept, as `flush`.
pub(super) fn frame(
    content: &[String],
    room: usize,
    scroll: &mut usize,
    top: &mut Option<usize>,
) -> Vec<String> {
    let lines = content.iter().enumerate().map(|(n, s)| {
        let line = Line::from(s.clone());
        (screen::Ready { line, width: 80 }, n)
    });
    let at = top.map(|t| (move |n: &usize| n.cmp(&t), 0));
    let (rows, s) = screen::window_tagged(lines, room, *scroll, at, usize::MAX);
    *scroll = s;
    *top = rows.first().map(|r| r.1).filter(|_| s > 0);
    rows.into_iter().map(|r| plain(&r.0)).collect()
}

// A lane just real enough to settle (real transcript/agent, `Run::Running`
// standing in for the job); the temp-dir guard comes back so it stays alive.
pub(super) fn a_running_lane() -> (tempfile::TempDir, Lane) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let lane = running_lane(dir.path());
    (dir, lane)
}

pub(super) fn running_lane(dir: &std::path::Path) -> Lane {
    struct Mute;
    #[async_trait::async_trait]
    impl llm::Transport for Mute {
        async fn stream(
            &self,
            _spec: &llm::model::ModelSpec,
            _req: &llm::request::Request,
        ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<llm::stream::StreamEvent>>>
        {
            Ok(Box::pin(futures::stream::empty()))
        }
    }
    let ws = tool::Workspace::new(dir).expect("a workspace");
    let spec = llm::model::ModelSpec {
        model: "m".into(),
        base_url: "http://localhost".into(),
        format: llm::model::Format::Anthropic {
            cache_control: llm::model::CacheControl::Off,
        },
        context_window: 200_000,
        max_output_tokens: 8_000,
        vision: false,
        accepts_effort: true,
        accepts_temperature: true,
        can_force_tool: true,
        pricing: llm::model::Pricing::default(),
    };
    let mut lane = Lane::opened(pi_core::core::lane::Opening {
        id: "s1".into(),
        ..pi_core::core::lane::Opening::new(
            std::sync::Arc::new(agent::Agent::new(std::sync::Arc::new(Mute), spec)),
            pi_core::core::lane::a_resolved(""),
            tool::Ctx::new(ws),
        )
    });
    // What every `start_*` leaves behind while its job runs.
    lane.begin(tokio_util::sync::CancellationToken::new());
    lane
}

pub(super) fn surface(dir: &std::path::Path) -> crate::tui::Tui {
    let keys = std::sync::Arc::new(Keys::default());
    let core = Core {
        store: Store::new(dir.join("state")),
        keys: keys.clone(),
        config: std::sync::Arc::new(pi_store::config::Config::default()),
        pinned: pi_core::args::Pinned::default(),
        commands: std::sync::Arc::new(Vec::new()),
        prompts_seen: 0,
        settings: pi_store::settings::Settings::new(toml::Value::Table(Default::default()), None),
        lanes: vec![running_lane(dir)],
        refused: Default::default(),
        jobs: None,
        current: 0,
    };
    crate::tui::Tui::on_test_screen(core, keys)
}

// Put another lane in front, the way a checkout switch would, and
// reconcile the surface against the lane it displaced.
pub(super) fn switch_to(tui: &mut crate::tui::Tui, lane: Lane) {
    let was = tui.core.current;
    tui.core.lanes.push(lane);
    tui.core.current = tui.core.lanes.len() - 1;
    tui.reconcile(was);
}

pub(super) fn vim_ui() -> crate::tui::Ui {
    let mut ui = test_ui(80, 24);
    ui.set_vim(&pi_store::config::Vim {
        enabled: true,
        ..Default::default()
    });
    ui
}

pub(super) fn typed(c: char) -> crate::tui::TermEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

pub(super) fn ctrl(c: char) -> crate::tui::TermEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

pub(super) fn mode(ui: &crate::tui::Ui) -> Option<Mode> {
    ui.vim
}

pub(super) fn test_ui(width: u16, height: u16) -> crate::tui::Ui {
    crate::tui::Ui::new(
        crate::tui::screen::Screen::test(width, height),
        std::sync::Arc::new(Keys::default()),
        Vec::new(),
        std::sync::Arc::new(Vec::new()),
        crate::tui::Lists::new(Store::new(std::env::temp_dir()), std::env::temp_dir()),
        Paint::new(true),
    )
}

// A lane as the bar holds one.
pub(super) fn tab(mark: crate::tui::Mark, name: &str) -> crate::tui::Tab {
    crate::tui::Tab {
        mark,
        name: name.into(),
    }
}

// The bar's row as text, without a screen to read it off.
pub(super) fn bar(ui: &crate::tui::Ui, width: usize) -> String {
    ratatui::text::Line::from(ui.tabs_strip(width)).to_string()
}
