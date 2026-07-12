use std::process::ExitCode;

use clap::Parser;
use pingora_reverse_proxy::config::{AppConfig, Cli, ConfigError};
use thiserror::Error;

#[derive(Debug, Error)]
enum StartupError {
    #[error(transparent)]
    Cli(#[from] clap::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(StartupError::Cli(error)) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            ExitCode::from(u8::try_from(exit_code).unwrap_or(1))
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), StartupError> {
    let cli = Cli::try_parse()?;
    let _config = AppConfig::try_from(cli)?;
    Ok(())
}
