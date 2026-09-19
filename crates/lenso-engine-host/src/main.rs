use clap::{Parser, Subcommand};
#[derive(Debug, Parser)]
struct Host {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    App {
        #[command(subcommand)]
        command: lenso_engine_app::app::AppCommand,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().skip(1).collect::<Vec<_>>() == ["--engine-host-info"] {
        println!(
            "{}",
            serde_json::json!({"schema":"lenso.engine-host.v1","target":lenso_engine_authoring::native_host_target()})
        );
        return Ok(());
    }
    let Command::App { command } = Host::parse().command;
    #[cfg(unix)]
    if matches!(
        &command,
        lenso_engine_app::app::AppCommand::Build(_)
            | lenso_engine_app::app::AppCommand::Assemble(_)
    ) {
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::spawn(async move {
            let code = tokio::select! { _=interrupt.recv()=>130, _=terminate.recv()=>143 };
            lenso_engine::process::terminate_active_processes();
            std::process::exit(code);
        });
    }
    lenso_engine_app::app::app(command).await
}
