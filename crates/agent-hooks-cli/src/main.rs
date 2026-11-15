mod cli;
mod commands;
mod paths;

use clap::Parser;
use std::io::Write;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // clap exits 2 on a parse error, which harnesses treat as a
            // BLOCKING hook error: an installed hook line with a flag this
            // binary does not know must fail open instead. `--help` and
            // `--version` are not errors and print as usual.
            let is_hook = std::env::args_os()
                .skip(1)
                .find(|arg| !arg.to_string_lossy().starts_with('-'))
                .is_some_and(|arg| arg == "hook");
            if is_hook && error.use_stderr() {
                let _ = writeln!(std::io::stderr(), "agent-hooks: {error}");
                std::process::exit(0);
            }
            error.exit()
        }
    };

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
            let return_event = args.return_event;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let socket = args
                .socket
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(agent_hooks_client::default_socket_path);
            match rt.block_on(commands::answer::run(socket, args)) {
                Err(error) if return_event => {
                    eprintln!("agent-hooks answer --return: {error:#}");
                    std::process::exit(2);
                }
                result => result,
            }
        }
        Commands::Start(args) => commands::agent::launch(commands::agent::Mode::Start, args),
        Commands::Exec(args) => commands::agent::launch(commands::agent::Mode::Exec, args),
        Commands::Models(args) => commands::agent::models(args),
        Commands::Install(args) => commands::install::run(args),
        Commands::SelfInstall(args) => commands::self_install::run(args),
        Commands::Uninstall(args) => commands::uninstall::run(args),
        Commands::Status(args) => commands::status::run(args),
        Commands::ListAdapters(args) => commands::list::run(args),
        Commands::Plugin(args) => commands::plugin::run(args),
        Commands::History(args) => commands::history::run(args),
    }
}
