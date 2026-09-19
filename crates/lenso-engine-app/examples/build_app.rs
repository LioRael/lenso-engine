//! Embeds App authoring directly; no CLI command parser or CLI executable needed.
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        anyhow::bail!("usage: build_app PROJECT OUTPUT PRECOMPILED_HOST");
    }
    lenso_engine_app::app::build_local((&args[0]).into(), (&args[1]).into(), (&args[2]).into())
}
