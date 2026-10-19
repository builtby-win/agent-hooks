mod cli;
mod commands;
mod paths;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Hook(args) => commands::hook::run(args),
        Commands::Init(args) => commands::user_hooks::cmd_init(args),
        Commands::Adapter(args) => commands::adapter::run(args),
        Commands::Daemon(args) => {
            // Daemon is async — block on runtime.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(commands::daemon::run(args))
        }
        Commands::Dev(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(commands::dev::run(args))
        }
        Commands::Answer(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let socket = args
                .socket
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(agent_hooks_client::default_socket_path);
            rt.block_on(commands::answer::run(socket, args.auto, args.line))
        }
        Commands::Start(args) => commands::agent::launch(commands::agent::Mode::Start, args),
        Commands::Exec(args) => commands::agent::launch(commands::agent::Mode::Exec, args),
        Commands::Models(args) => commands::agent::models(args),
        Commands::Install(args) => commands::install::run(args),
        Commands::Uninstall(args) => commands::uninstall::run(args),
        Commands::Status(args) => commands::status::run(args),
        Commands::ListAdapters(args) => commands::list::run(args),
        Commands::Plugin(args) => commands::plugin::run(args),
    }
}
