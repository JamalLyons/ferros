use clap::Parser;
use ferros_core::prelude::*;

use crate::cli::Cli;
use crate::commands::{Commands, run_attach_command};

mod cli;
mod commands;

fn main() -> FerrosResult<()>
{
    let cli = Cli::parse();
    run_command(cli)?;
    Ok(())
}

fn run_command(cli: Cli) -> FerrosResult<()>
{
    match cli.command {
        Commands::Attach { pid } => {
            run_attach_command(pid)?;
            Ok(())
        }
        _ => Err(FerrosError::InvalidArgument("Invalid command".to_string())),
    }
}
