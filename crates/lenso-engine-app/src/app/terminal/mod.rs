// Generated projections of the terminal-owned contracts. See assets/terminal/README.md.
#[rustfmt::skip]
#[allow(clippy::all, dead_code)]
pub mod command;
#[rustfmt::skip]
#[allow(clippy::all, dead_code)]
pub mod provider;

mod parser;

pub async fn run(app: &lenso_kernel::NativeApp, args: &[String]) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use command::{CatalogRequest, CommandCatalog, CommandExecute, ExecuteOpen, OutputKind};
    use lenso_kernel::StreamEvent;
    // This is an installed Plugin instance with an ordinary Plan-bound dependency.
    // The ingress cannot invoke arbitrary providers or create its own bindings.
    let caller = "lenso.terminal.cli/default";
    let catalog = app
        .handle::<CommandCatalog>(caller)
        .map_err(|e| anyhow::anyhow!("CLI support is not adopted: {e:?}"))?
        .invoke("catalog", CatalogRequest {})
        .await
        .map_err(|e| anyhow::anyhow!("terminal catalog: {e:?}"))?
        .map_err(|e| anyhow::anyhow!("terminal catalog: {e:?}"))?;
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        for command in catalog.commands {
            println!("{}\t{}", command.path.join(" "), command.summary);
        }
        return Ok(());
    }
    let parsed = match parser::parse_args(&catalog.commands, "app", args)? {
        parser::ParseOutcome::Help(help) => {
            print!("{help}");
            return Ok(());
        }
        parser::ParseOutcome::NoMatch => bail!("unknown App command: {}", args.join(" ")),
        parser::ParseOutcome::Command(command) => command,
    };
    let stream = app
        .stream_handle::<CommandExecute>(caller)
        .map_err(|e| anyhow::anyhow!("terminal stream: {e:?}"))?
        .open(
            "execute",
            ExecuteOpen {
                id: parsed.id,
                arguments_json: parsed
                    .arguments_json
                    .try_into()
                    .context("terminal arguments")?,
                output_format: parsed.output_format,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("terminal open: {e:?}"))?
        .map_err(|e| anyhow::anyhow!("terminal command: {e:?}"))?;
    stream
        .close_send()
        .await
        .map_err(|e| anyhow::anyhow!("terminal half-close: {e:?}"))?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async {
        #[cfg(unix)]
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await
    };
    tokio::pin!(shutdown);
    let mut bytes = 0usize;
    loop {
        let event = tokio::select! {
            event = stream.receive() => event.map_err(|e| anyhow::anyhow!("terminal receive: {e:?}"))?,
            signal = &mut shutdown => { signal?; stream.cancel(); bail!("App command cancelled"); }
        };
        match event {
            StreamEvent::Message(message) => {
                bytes += message.content.len();
                if bytes > 16 * 1024 * 1024 {
                    stream.cancel();
                    bail!("terminal output exceeds 16 MiB");
                }
                use std::io::Write;
                if matches!(message.kind, OutputKind::Stderr) {
                    std::io::stderr().write_all(message.content.as_bytes())?;
                } else {
                    std::io::stdout().write_all(message.content.as_bytes())?;
                }
            }
            StreamEvent::PeerHalfClosed => {}
            StreamEvent::Terminal(Ok(())) => return Ok(()),
            StreamEvent::Terminal(Err(error)) => bail!("App command failed: {error:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_projections_match_owned_contract_snapshots() {
        use lenso_contract_codegen_next::{ProjectionLanguage, generate_projection};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for role in ["command", "provider"] {
            let descriptor = root.join(format!("assets/terminal/contracts/{role}/capability.json"));
            for (language, output) in [
                (
                    ProjectionLanguage::RustRuntime,
                    format!("src/app/terminal/{role}.rs"),
                ),
                (
                    ProjectionLanguage::TypeScript,
                    format!("assets/terminal/{role}.ts"),
                ),
            ] {
                assert_eq!(
                    generate_projection(&descriptor, language).unwrap().source,
                    std::fs::read_to_string(root.join(output)).unwrap()
                );
            }
        }
    }
}
