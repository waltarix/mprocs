use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Options {
  /// Config path
  #[arg(short = 'c', long = "config", value_name = "PATH")]
  pub config: Option<PathBuf>,

  /// Remote control server address. Example: 127.0.0.1:4050.
  #[arg(short = 's', long = "server", value_name = "HOST:PORT")]
  pub server: Option<String>,

  /// Send yaml/json encoded command to running mprocs
  #[arg(long = "ctl")]
  pub control: Option<String>,

  /// Names for processes provided by cli arguments. Separated by comma.
  #[arg(long = "names")]
  pub names: Option<String>,

  /// Run scripts from package.json. Scripts are not started by default.
  #[arg(long = "npm")]
  pub npm: bool,

  /// Commands to run (if omitted, commands from config will be run)
  pub commands: Vec<String>,
}
