use tui::{
  style::{Color, Modifier, Style},
  widgets::{Block, Borders},
};

pub struct Theme {
  pub procs_item: Style,
  pub procs_item_active: Style,
}

impl Theme {
  pub fn style(&self, active: bool) -> Style {
    match active {
      true => Style::default().fg(Color::Rgb(0, 255, 127)),
      false => Style::default().fg(Color::Rgb(192, 192, 192)),
    }
  }

  pub fn pane(&self, active: bool) -> Block {
    let style = self.style(active);

    Block::default().borders(Borders::ALL).border_style(style)
  }

  pub fn copy_mode_label(&self) -> Style {
    Style::default()
      .fg(Color::Black)
      .bg(Color::Yellow)
      .add_modifier(Modifier::BOLD)
  }

  pub fn get_procs_item(&self, active: bool) -> Style {
    if active {
      self.procs_item_active
    } else {
      self.procs_item
    }
  }

  pub fn zoom_tip(&self) -> Style {
    Style::default().fg(Color::Black).bg(Color::Yellow)
  }
}

impl Default for Theme {
  fn default() -> Self {
    Self {
      procs_item: Style::default().fg(Color::Reset),
      procs_item_active: Style::default().bg(Color::Rgb(56, 58, 62)),
    }
  }
}
