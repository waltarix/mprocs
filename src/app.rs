use std::{io, rc::Rc};

use crossterm::{
  event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode,
    KeyEvent, MouseButton, MouseEventKind,
  },
  execute,
  terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
    LeaveAlternateScreen,
  },
};
use futures::{future::FutureExt, select, StreamExt};
use tokio::{
  io::AsyncReadExt,
  sync::mpsc::{UnboundedReceiver, UnboundedSender},
};
use tui::{
  backend::CrosstermBackend,
  layout::{Constraint, Direction, Layout, Margin, Rect},
  Terminal,
};
use tui_input::Input;

use crate::{
  clipboard::copy,
  config::{CmdConfig, Config, ProcConfig, ServerConfig},
  event::{AppEvent, CopyMove},
  key::Key,
  keymap::Keymap,
  proc::{CopyMode, Pos, Proc, ProcState, ProcUpdate, StopSignal},
  state::{Modal, Scope, State},
  ui_add_proc::render_add_proc,
  ui_confirm_quit::render_confirm_quit,
  ui_keymap::render_keymap,
  ui_procs::{procs_check_hit, procs_get_clicked_index, render_procs},
  ui_remove_proc::render_remove_proc,
  ui_term::{render_term, term_check_hit},
  ui_zoom_tip::render_zoom_tip,
};

type Term = Terminal<CrosstermBackend<io::Stdout>>;

enum LoopAction {
  Render,
  Skip,
  ForceQuit,
}

pub struct App {
  config: Config,
  keymap: Rc<Keymap>,
  terminal: Term,
  state: State,
  upd_rx: UnboundedReceiver<(usize, ProcUpdate)>,
  upd_tx: UnboundedSender<(usize, ProcUpdate)>,
  ev_rx: UnboundedReceiver<AppEvent>,
  ev_tx: UnboundedSender<AppEvent>,
}

impl App {
  pub fn from_config_file(
    config: Config,
    keymap: Keymap,
  ) -> anyhow::Result<Self> {
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;

    let (upd_tx, upd_rx) =
      tokio::sync::mpsc::unbounded_channel::<(usize, ProcUpdate)>();
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let state = State {
      scope: Scope::Procs,
      procs: Vec::new(),
      selected: 0,

      modal: None,

      quitting: false,
    };

    let app = App {
      config,
      keymap: Rc::new(keymap),
      terminal,
      state,
      upd_rx,
      upd_tx,

      ev_rx,
      ev_tx,
    };
    Ok(app)
  }

  pub async fn run(self) -> anyhow::Result<()> {
    enable_raw_mode()?;

    let res = self.run_impl().await;

    disable_raw_mode()?;

    res
  }

  pub async fn run_impl(mut self) -> anyhow::Result<()> {
    execute!(io::stdout(), EnterAlternateScreen)?;
    self.terminal.clear()?;
    execute!(io::stdout(), EnableMouseCapture)?;

    let (exit_trigger, exit_listener) = triggered::trigger();

    let server_thread = if let Some(ref server_addr) = self.config.server {
      let server = match server_addr {
        ServerConfig::Tcp(addr) => tokio::net::TcpListener::bind(addr).await?,
      };

      let ev_tx = self.ev_tx.clone();
      let server_thread = tokio::spawn(async move {
        loop {
          let on_exit = exit_listener.clone();
          let mut socket: tokio::net::TcpStream = select! {
            _ = on_exit.fuse() => break,
            client = server.accept().fuse() => {
              if let Ok((socket, _)) = client {
                socket
              } else {
                break;
              }
            }
          };

          let ctl_tx = ev_tx.clone();
          let on_exit = exit_listener.clone();
          tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            select! {
              _ = on_exit.fuse() => return,
              count = socket.read_to_end(&mut buf).fuse() => {
                if count.is_err() {
                  return;
                }
              }
            };
            let msg: AppEvent = serde_yaml::from_slice(buf.as_slice()).unwrap();
            // log::info!("Received remote command: {:?}", msg);
            ctl_tx.send(msg).unwrap();
          });
        }
      });
      Some(server_thread)
    } else {
      None
    };

    let result = self.main_loop().await;

    exit_trigger.trigger();
    if let Some(server_thread) = server_thread {
      let _ = server_thread.await;
    }

    execute!(io::stdout(), DisableMouseCapture)?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
  }

  async fn main_loop(mut self) -> anyhow::Result<()> {
    let mut input = EventStream::new();

    let mut last_term_size = {
      let area = self.get_layout().term_area();
      self.start_procs(area)?;
      (area.width, area.height)
    };

    let mut render_needed = true;
    loop {
      if render_needed {
        self.terminal.draw(|f| {
          let layout = AppLayout::new(
            f.size(),
            self.state.scope.is_zoomed(),
            &self.config,
          );

          {
            let term_area = layout.term_area();
            let term_size = (term_area.width, term_area.height);
            if last_term_size != term_size {
              last_term_size = term_size;
              for proc in &mut self.state.procs {
                proc.resize(term_area);
              }
            }
          }

          render_procs(layout.procs, f, &mut self.state);
          render_term(layout.term, f, &mut self.state);
          render_keymap(layout.keymap, f, &mut self.state, &self.keymap);
          render_zoom_tip(layout.zoom_banner, f, &self.keymap);

          if let Some(modal) = &mut self.state.modal {
            match modal {
              Modal::AddProc { input } => {
                render_add_proc(f.size(), f, input);
              }
              Modal::RemoveProc { id: _ } => {
                render_remove_proc(f.size(), f);
              }
              Modal::Quit => {
                render_confirm_quit(f.size(), f);
              }
            }
          }
        })?;
      }

      let loop_action = select! {
        event = input.next().fuse() => {
          self.handle_input(event)
        }
        event = self.upd_rx.recv().fuse() => {
          if let Some(event) = event {
            self.handle_proc_update(event)
          } else {
            LoopAction::Skip
          }
        }
        event = self.ev_rx.recv().fuse() => {
          if let Some(event) = event {
            self.handle_event(&event)
          } else {
            LoopAction::Skip
          }
        }
      };

      if self.state.quitting && self.state.all_procs_down() {
        break;
      }

      match loop_action {
        LoopAction::Render => {
          render_needed = true;
        }
        LoopAction::Skip => {
          render_needed = false;
        }
        LoopAction::ForceQuit => break,
      };
    }

    Ok(())
  }

  fn start_procs(&mut self, size: Rect) -> anyhow::Result<()> {
    let mut procs = self
      .config
      .procs
      .iter()
      .map(|proc_cfg| {
        Proc::new(proc_cfg.name.clone(), proc_cfg, self.upd_tx.clone(), size)
      })
      .collect::<Vec<_>>();

    self.state.procs.append(&mut procs);

    Ok(())
  }

  fn handle_input(
    &mut self,
    event: Option<crossterm::Result<Event>>,
  ) -> LoopAction {
    let event = match event {
      Some(Ok(event)) => event,
      Some(Err(err)) => {
        log::warn!("Crossterm input error: {}", err.to_string());
        return LoopAction::Skip;
      }
      None => {
        log::warn!("Crossterm input is None.");
        return LoopAction::Skip;
      }
    };

    {
      let mut ret: Option<LoopAction> = None;
      let mut reset_modal = false;
      if let Some(modal) = &mut self.state.modal {
        match modal {
          Modal::AddProc { input } => {
            match event {
              Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                self
                  .ev_tx
                  .send(AppEvent::AddProc {
                    cmd: input.value().to_string(),
                  })
                  .unwrap();
                // Skip because AddProc event will immediately rerender.
                ret = Some(LoopAction::Skip);
              }
              Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                ret = Some(LoopAction::Render);
              }
              _ => (),
            }

            let req = tui_input::backend::crossterm::to_input_request(event);
            if let Some(req) = req {
              input.handle(req);
              ret = Some(LoopAction::Render);
            }
          }
          Modal::RemoveProc { id } => {
            match event {
              Event::Key(KeyEvent {
                code: KeyCode::Char('y'),
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                self.ev_tx.send(AppEvent::RemoveProc { id: *id }).unwrap();
                // Skip because RemoveProc event will immediately rerender.
                ret = Some(LoopAction::Skip);
              }
              Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers,
              })
              | Event::Key(KeyEvent {
                code: KeyCode::Char('n'),
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                ret = Some(LoopAction::Render);
              }
              _ => (),
            }
          }
          Modal::Quit => match event {
            Event::Key(KeyEvent {
              code: KeyCode::Char('y'),
              modifiers,
            }) if modifiers.is_empty() => {
              reset_modal = true;
              self.ev_tx.send(AppEvent::Quit).unwrap();
              ret = Some(LoopAction::Skip);
            }
            Event::Key(KeyEvent {
              code: KeyCode::Esc,
              modifiers,
            })
            | Event::Key(KeyEvent {
              code: KeyCode::Char('n'),
              modifiers,
            }) if modifiers.is_empty() => {
              reset_modal = true;
              ret = Some(LoopAction::Render);
            }
            _ => (),
          },
        };
      }

      if reset_modal {
        self.state.modal = None;
      }
      if let Some(ret) = ret {
        return ret;
      }
    }

    match event {
      Event::Key(key) => {
        let key = Key::from(key);
        let keymap = self.keymap.clone();
        let group = self.state.get_keymap_group();
        if let Some(bound) = keymap.resolve(group, &key) {
          self.handle_event(bound)
        } else {
          match self.state.scope {
            Scope::Procs => LoopAction::Skip,
            Scope::Term | Scope::TermZoom => {
              self.handle_event(&AppEvent::SendKey { key })
            }
          }
        }
      }
      Event::Mouse(mev) => {
        if mev.kind == MouseEventKind::Moved {
          return LoopAction::Skip;
        }

        let layout = self.get_layout();
        if term_check_hit(layout.term_area(), mev.column, mev.row) {
          if let (Scope::Procs, MouseEventKind::Down(_)) =
            (self.state.scope, mev.kind)
          {
            self.state.scope = Scope::Term
          }
          if let Some(proc) = self.state.get_current_proc_mut() {
            proc.handle_mouse(mev, layout.term_area(), &self.config);
          }
        } else if procs_check_hit(layout.procs, mev.column, mev.row) {
          if let (Scope::Term, MouseEventKind::Down(_)) =
            (self.state.scope, mev.kind)
          {
            self.state.scope = Scope::Procs
          }
          match mev.kind {
            MouseEventKind::Down(btn) => match btn {
              MouseButton::Left => {
                if let Some(index) = procs_get_clicked_index(
                  layout.procs,
                  mev.column,
                  mev.row,
                  &self.state,
                ) {
                  self.state.select_proc(index);
                }
              }
              MouseButton::Right | MouseButton::Middle => (),
            },
            MouseEventKind::Up(_) => (),
            MouseEventKind::Drag(_) => (),
            MouseEventKind::Moved => (),
            MouseEventKind::ScrollDown => {
              if self.state.selected < self.state.procs.len().saturating_sub(1)
              {
                let index = self.state.selected + 1;
                self.state.select_proc(index);
              }
            }
            MouseEventKind::ScrollUp => {
              if self.state.selected > 0 {
                let index = self.state.selected - 1;
                self.state.select_proc(index);
              }
            }
          }
        }
        LoopAction::Render
      }
      Event::Resize(width, height) => {
        let (width, height) = if cfg!(windows) {
          crossterm::terminal::size().unwrap()
        } else {
          (width, height)
        };

        let area = AppLayout::new(
          Rect::new(0, 0, width, height),
          self.state.scope.is_zoomed(),
          &self.config,
        )
        .term_area();
        for proc in &mut self.state.procs {
          proc.resize(area);
        }

        LoopAction::Render
      }
    }
  }

  fn handle_event(&mut self, event: &AppEvent) -> LoopAction {
    match event {
      AppEvent::Batch { cmds } => {
        let mut ret = LoopAction::Skip;
        for cmd in cmds {
          match self.handle_event(cmd) {
            LoopAction::Render => ret = LoopAction::Render,
            LoopAction::Skip => (),
            LoopAction::ForceQuit => return LoopAction::ForceQuit,
          };
        }
        ret
      }

      AppEvent::QuitOrAsk => {
        let have_running = self.state.procs.iter().any(|p| p.is_up());
        if have_running {
          self.state.modal = Some(Modal::Quit);
        } else {
          self.state.quitting = true;
        }
        LoopAction::Render
      }
      AppEvent::Quit => {
        self.state.quitting = true;
        for proc in self.state.procs.iter_mut() {
          if proc.is_up() {
            proc.stop();
          }
        }
        LoopAction::Render
      }
      AppEvent::ForceQuit => {
        for proc in self.state.procs.iter_mut() {
          if proc.is_up() {
            proc.kill();
          }
        }
        LoopAction::ForceQuit
      }

      AppEvent::ToggleFocus => {
        self.state.scope = self.state.scope.toggle();
        LoopAction::Render
      }
      AppEvent::FocusProcs => {
        self.state.scope = Scope::Procs;
        LoopAction::Render
      }
      AppEvent::FocusTerm => {
        self.state.scope = Scope::Term;
        LoopAction::Render
      }
      AppEvent::Zoom => {
        self.state.scope = Scope::TermZoom;
        LoopAction::Render
      }

      AppEvent::NextProc => {
        let mut next = self.state.selected + 1;
        if next >= self.state.procs.len() {
          next = 0;
        }
        self.state.select_proc(next);
        LoopAction::Render
      }
      AppEvent::PrevProc => {
        let next = if self.state.selected > 0 {
          self.state.selected - 1
        } else {
          self.state.procs.len() - 1
        };
        self.state.select_proc(next);
        LoopAction::Render
      }
      AppEvent::SelectProc { index } => {
        self.state.select_proc(*index);
        LoopAction::Render
      }

      AppEvent::StartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.start();
        }
        LoopAction::Skip
      }
      AppEvent::TermProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.stop();
        }
        LoopAction::Skip
      }
      AppEvent::KillProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.kill();
        }
        LoopAction::Skip
      }
      AppEvent::RestartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if proc.is_up() {
            proc.stop();
            proc.to_restart = true;
          } else {
            proc.start();
          }
        }
        LoopAction::Skip
      }
      AppEvent::ForceRestartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if proc.is_up() {
            proc.kill();
            proc.to_restart = true;
          } else {
            proc.start();
          }
        }
        LoopAction::Skip
      }

      AppEvent::ScrollUpLines { n } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_up_lines(*n);
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollDownLines { n } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_down_lines(*n);
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollUp => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_half_screen_up();
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollDown => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_half_screen_down();
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ShowAddProc => {
        self.state.modal = Some(Modal::AddProc {
          input: Input::default(),
        });
        LoopAction::Render
      }
      AppEvent::AddProc { cmd } => {
        let proc = Proc::new(
          cmd.to_string(),
          &ProcConfig {
            name: cmd.to_string(),
            cmd: CmdConfig::Shell {
              shell: cmd.to_string(),
            },
            cwd: None,
            env: None,
            autostart: true,
            stop: StopSignal::default(),
          },
          self.upd_tx.clone(),
          self.get_layout().term_area(),
        );
        self.state.procs.push(proc);
        LoopAction::Render
      }
      AppEvent::ShowRemoveProc => {
        let id = self.state.get_current_proc().and_then(|proc| {
          if proc.is_up() {
            None
          } else {
            Some(proc.id)
          }
        });
        match id {
          Some(id) => {
            self.state.modal = Some(Modal::RemoveProc { id });
            LoopAction::Render
          }
          None => LoopAction::Skip,
        }
      }
      AppEvent::RemoveProc { id } => {
        self
          .state
          .procs
          .retain(|proc| proc.is_up() || proc.id != *id);
        LoopAction::Render
      }

      AppEvent::CopyModeEnter => {
        let switched = match self.state.get_current_proc_mut() {
          Some(proc) => match &mut proc.inst {
            ProcState::None => false,
            ProcState::Some(inst) => {
              let screen = inst.vt.read().unwrap().screen().clone();
              let y = (screen.size().0 - 1) as i32;
              proc.copy_mode = CopyMode::Start(screen, Pos { y, x: 0 });
              true
            }
            ProcState::Error(_) => false,
          },
          None => false,
        };
        if switched {
          self.state.scope = Scope::Term;
        }
        LoopAction::Render
      }
      AppEvent::CopyModeLeave => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.copy_mode = CopyMode::None(None);
        }
        LoopAction::Render
      }
      AppEvent::CopyModeMove { dir } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          match &proc.inst {
            ProcState::None => (),
            ProcState::Some(inst) => {
              let vt = inst.vt.read().unwrap();
              let screen = vt.screen();
              match &mut proc.copy_mode {
                CopyMode::None(_) => (),
                CopyMode::Start(_, pos_) | CopyMode::Range(_, _, pos_) => {
                  match dir {
                    CopyMove::Up => {
                      if pos_.y > -(screen.scrollback_len() as i32) {
                        pos_.y -= 1
                      }
                    }
                    CopyMove::Right => {
                      if pos_.x + 1 < screen.size().1 as i32 {
                        pos_.x += 1
                      }
                    }
                    CopyMove::Left => {
                      if pos_.x > 0 {
                        pos_.x -= 1
                      }
                    }
                    CopyMove::Down => {
                      if pos_.y + 1 < screen.size().0 as i32 {
                        pos_.y += 1
                      }
                    }
                  };
                }
              }
            }
            ProcState::Error(_) => (),
          }
        }
        LoopAction::Render
      }
      AppEvent::CopyModeEnd => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.copy_mode = match std::mem::take(&mut proc.copy_mode) {
            CopyMode::Start(screen, start) => {
              CopyMode::Range(screen, start.clone(), start)
            }
            other => other,
          };
        }
        LoopAction::Render
      }
      AppEvent::CopyModeCopy => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if let CopyMode::Range(screen, start, end) = &proc.copy_mode {
            let (low, high) = Pos::to_low_high(start, end);
            let text = screen.get_selected_text(low.x, low.y, high.x, high.y);

            copy(text.as_str());
          }
          proc.copy_mode = CopyMode::None(None);
        }
        LoopAction::Render
      }

      AppEvent::SendKey { key } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.send_key(key);
        }
        LoopAction::Skip
      }
    }
  }

  fn handle_proc_update(&mut self, event: (usize, ProcUpdate)) -> LoopAction {
    match event.1 {
      ProcUpdate::Render => {
        let cur_proc_id =
          self.state.get_current_proc().map_or(usize::MAX, |p| p.id);
        if let Some(proc) = self.state.get_proc_mut(event.0) {
          if proc.id != cur_proc_id {
            proc.changed = true;
          }
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      ProcUpdate::Stopped => {
        if let Some(proc) = self.state.get_proc_mut(event.0) {
          if proc.to_restart {
            proc.start();
            proc.to_restart = false;
          }
        }
        LoopAction::Render
      }
      ProcUpdate::Started => LoopAction::Render,
    }
  }

  fn get_layout(&mut self) -> AppLayout {
    AppLayout::new(
      self.terminal.get_frame().size(),
      self.state.scope.is_zoomed(),
      &self.config,
    )
  }
}

struct AppLayout {
  procs: Rect,
  term: Rect,
  keymap: Rect,
  zoom_banner: Rect,
}

impl AppLayout {
  pub fn new(area: Rect, zoom: bool, config: &Config) -> Self {
    let keymap_h = if zoom || config.hide_keymap_window {
      0
    } else {
      3
    };
    let procs_w = if zoom {
      0
    } else {
      config.proc_list_width as u16
    };
    let zoom_banner_h = u16::from(zoom);
    let top_bot = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Min(1), Constraint::Length(keymap_h)])
      .split(area);
    let chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Length(procs_w), Constraint::Min(2)].as_ref())
      .split(top_bot[0]);
    let term_zoom = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Length(zoom_banner_h), Constraint::Min(1)])
      .split(chunks[1]);

    Self {
      procs: chunks[0],
      term: term_zoom[1],
      keymap: top_bot[1],
      zoom_banner: term_zoom[0],
    }
  }

  pub fn term_area(&self) -> Rect {
    self.term.inner(&Margin {
      vertical: 1,
      horizontal: 1,
    })
  }
}
