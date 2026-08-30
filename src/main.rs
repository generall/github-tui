mod app;
mod backend;
mod md;
mod ui;
mod upgrade;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::sync::mpsc;
use std::time::Duration;

const USAGE: &str = "usage: github-tui [owner/repo[#N] | N | self-upgrade | --version]

With no argument: the repo of the current directory (and the PR of the
checked-out branch, if any), else the last used repo.";

fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("self-upgrade") => return upgrade::self_upgrade(),
        Some("--version" | "-V") => {
            println!("github-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(a) if a.starts_with('-') => {
            eprintln!("unknown argument: {a}\n{USAGE}");
            std::process::exit(2);
        }
        _ => {}
    }
    let cwd_repo = backend::cwd_repo();
    let mut terminal = ratatui::init();
    let (tx, rx) = mpsc::channel();
    let mut app = app::App::new(tx, cwd_repo, arg);

    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if let Some(ext) = app.external.take() {
                let res = run_external_editor(&mut terminal, &ext.template)?;
                app.on_external(ext.req, res);
                continue;
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                    _ => {}
                }
            }
            while let Ok(msg) = rx.try_recv() {
                app.on_msg(msg);
            }
            app.tick();
            if app.quit {
                return Ok(());
            }
        }
    })();

    ratatui::restore();
    app.cache.save();
    result
}

/// Suspend the TUI, run $VISUAL/$EDITOR/vi on a temp file seeded with
/// `initial` (git-commit style), resume, and return the edited content.
/// `None` = the editor exited non-zero (abort).
fn run_external_editor(
    terminal: &mut ratatui::DefaultTerminal,
    initial: &str,
) -> Result<Option<String>> {
    let path = std::env::temp_dir().join(format!("github-tui-{}.md", std::process::id()));
    std::fs::write(&path, initial)?;

    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} '{}'", path.display()))
        .status();
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    terminal.clear()?;

    let content = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    Ok(match status {
        Ok(s) if s.success() => content,
        _ => None,
    })
}
