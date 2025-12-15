use clap::Subcommand;
use ferros_core::prelude::*;

#[derive(Subcommand, Debug)]
pub enum Commands
{
    /// Attach to a running process by PID
    Attach
    {
        /// Process ID (PID) to attach to
        pid: u32,
    },
    /// Launch a new process under debugger control
    Launch
    {
        /// Path to the executable to launch
        program: String,
        /// Arguments to pass to the program
        /// Note: To set Ferros log level, use --log-level before the 'launch' subcommand:
        ///   ferros --log-level debug launch <program>
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub fn run_attach_command(pid: u32) -> FerrosResult<()>
{
    info!("Attaching to process {}", pid);
    let mut debugger = create_debugger()?;
    debugger.attach(ProcessId::from(pid))?;
    info!("Successfully attached to process {}", pid);
    Ok(())
}
