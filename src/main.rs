mod app;
mod cli;
mod clipboard;
mod config;
mod config_lua;
mod ctl;
mod encode_term;
mod error;
mod event;
mod key;
mod keymap;
mod package_json;
mod proc;
mod settings;
mod state;
mod theme;
mod ui_add_proc;
mod ui_confirm_quit;
mod ui_keymap;
mod ui_procs;
mod ui_remove_proc;
mod ui_term;
mod ui_zoom_tip;
mod yaml_val;

use std::{
  io::Read,
  path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use clap::Parser;
use cli::Options;
use config::{CmdConfig, Config, ConfigContext, ProcConfig, ServerConfig};
use config_lua::load_lua_config;
use ctl::run_ctl;
use flexi_logger::FileSpec;
use keymap::Keymap;
use package_json::load_npm_procs;
use proc::StopSignal;
use serde_yaml::Value;
use settings::Settings;
use yaml_val::Val;

use crate::app::App;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
  let logger_str = if cfg!(debug_assertions) {
    "info"
  } else {
    "warn"
  };
  let _logger = flexi_logger::Logger::try_with_str(logger_str)
    .unwrap()
    .log_to_file(FileSpec::default().suppress_timestamp())
    .append()
    .use_utc()
    .start()
    .unwrap();

  match run_app().await {
    Ok(()) => Ok(()),
    Err(err) => {
      eprintln!("Error: {}", err);
      Ok(())
    }
  }
}

async fn run_app() -> anyhow::Result<()> {
  let options = Options::parse();

  let config_value = load_config_value(options.config)
    .map_err(|e| anyhow::Error::msg(format!("[{}] {}", "config", e)))?;

  let mut settings = Settings::default();

  // merge ~/.config/mprocs/mprocs.yaml
  settings.merge_from_xdg().map_err(|e| {
    anyhow::Error::msg(format!("[{}] {}", "global settings", e))
  })?;
  // merge ./mprocs.yaml
  if let Some((value, _)) = &config_value {
    settings
      .merge_value(Val::new(value)?)
      .map_err(|e| anyhow::Error::msg(format!("[{}] {}", "local config", e)))?;
  }

  let mut keymap = Keymap::new();
  settings.add_to_keymap(&mut keymap)?;

  let config = {
    let mut config = if let Some((v, ctx)) = config_value {
      Config::from_value(&v, &ctx, &settings)?
    } else {
      Config::make_default(&settings)
    };

    if let Some(server_addr) = options.server {
      config.server = Some(ServerConfig::Tcp(server_addr));
    }

    if let Some(control) = options.control {
      return run_ctl(control.as_str(), &config).await;
    }

    if !options.commands.is_empty() {
      let names = options.names.map_or_else(Vec::new, |arg| {
        arg.split(',').map(|a| a.to_string()).collect::<Vec<_>>()
      });
      let procs = options
        .commands
        .into_iter()
        .enumerate()
        .map(|(i, cmd)| ProcConfig {
          name: names
            .get(i)
            .map_or_else(|| cmd.to_string(), |s| s.to_string()),
          cmd: CmdConfig::Shell { shell: cmd },
          env: None,
          cwd: None,
          autostart: true,
          stop: StopSignal::default(),
        })
        .collect::<Vec<_>>();

      config.procs = procs;
    } else if options.npm {
      let procs = load_npm_procs()?;
      config.procs = procs;
    }

    config
  };

  let app = App::from_config_file(config, keymap)?;
  app.run().await
}

fn load_config_value(
  config: Option<PathBuf>,
) -> Result<Option<(Value, ConfigContext)>> {
  if let Some(path) = config {
    return Ok(Some((
      read_value(path.to_str().unwrap())?,
      ConfigContext { path },
    )));
  }

  {
    let path = "mprocs.lua";
    if Path::new(path).is_file() {
      return Ok(Some((
        read_value(path)?,
        ConfigContext { path: path.into() },
      )));
    }
  }

  {
    let path = "mprocs.yaml";
    if Path::new(path).is_file() {
      return Ok(Some((
        read_value(path)?,
        ConfigContext { path: path.into() },
      )));
    }
  }

  {
    let path = "mprocs.json";
    if Path::new(path).is_file() {
      return Ok(Some((
        read_value(path)?,
        ConfigContext { path: path.into() },
      )));
    }
  }

  Ok(None)
}

fn read_value(path: &str) -> Result<Value> {
  // Open the file in read-only mode with buffer.
  let file = match std::fs::File::open(path) {
    Ok(file) => file,
    Err(err) => match err.kind() {
      std::io::ErrorKind::NotFound => {
        bail!("Config file '{}' not found.", path);
      }
      _kind => return Err(err.into()),
    },
  };
  let mut reader = std::io::BufReader::new(file);
  let ext = std::path::Path::new(path)
    .extension()
    .map_or_else(|| "".to_string(), |ext| ext.to_string_lossy().to_string());
  let value: Value = match ext.as_str() {
    "yaml" | "yml" => serde_yaml::from_reader(reader)?,
    "lua" => {
      let mut buf = String::new();
      reader.read_to_string(&mut buf)?;
      load_lua_config(path, &buf)?
    }
    _ => bail!("Supported config extensions: lua, yaml, yml."),
  };
  Ok(value)
}
