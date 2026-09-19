//! Execute only selected convention compilers. Discovery remains side-effect free.
use anyhow::bail;
use lenso_app_authoring::discovery::{
    Candidate,
    conventions::{ConventionPlan, generated_candidate},
};
use std::{fs, path::Path, process::Command};

pub(super) fn compile(plan: &ConventionPlan, output: &Path) -> anyhow::Result<Vec<Candidate>> {
    let compiler_group = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let mut candidates = Vec::new();
    for compilation in &plan.compilations {
        let project = output.join(&compilation.plugin_id);
        fs::create_dir(&project)?;
        let request = serde_json::json!({
            "schema":"lenso.convention-compile.v1", "entry":compilation.entry,
            "owner_project":compilation.owner_project, "plugin_id":compilation.plugin_id,
            "release_version":compilation.version, "convention":compilation.convention,
            "output": project,
        });
        let source_before = super::local_host::input_digest(&compilation.owner_project)?;
        let compiler_before = super::local_host::input_digest(&compilation.compiler_project)?;
        let mut engine = lenso_engine::Engine::default();
        engine.register(lenso_engine_runtime::RuntimeProcessor::new(
            crate::ConventionCompiler {
                identity: compilation.convention.clone(),
                request,
                process: lenso_engine::process::ProcessSpec {
                    program: compilation.compiler.program.clone(),
                    args: compilation.compiler.args.clone(),
                    directory: compilation.compiler_project.clone(),
                },
                active_group: compiler_group.clone(),
            },
        ))?;
        let processing = engine.plan(lenso_engine::Snapshot::default())?;
        engine.execute(&processing, &super::preset::cancellation())?;
        validate_tree(&project, &mut 0, &mut 0, 0)?;
        if source_before != super::local_host::input_digest(&compilation.owner_project)?
            || compiler_before != super::local_host::input_digest(&compilation.compiler_project)?
        {
            bail!("convention inputs changed during compilation; retry");
        }
        let candidate = generated_candidate(&project, compilation)?;
        if candidate.format == "bun" {
            let status = Command::new("bun")
                .args(["install", "--ignore-scripts"])
                .current_dir(&project)
                .status()?;
            if !status.success() {
                bail!("install selected convention output dependencies");
            }
        }
        if candidate.format == "cargo"
            && !Command::new("cargo")
                .arg("generate-lockfile")
                .current_dir(&project)
                .status()?
                .success()
        {
            bail!("resolve selected Rust convention output dependencies");
        }
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn validate_tree(
    path: &Path,
    files: &mut usize,
    bytes: &mut u64,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 32 {
        bail!("compiler output exceeds 32 directory levels");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.file_type()?;
        *files += 1;
        if meta.is_dir() {
            validate_tree(&entry.path(), files, bytes, depth + 1)?;
        } else if meta.is_file() {
            *bytes += entry.metadata()?.len();
        } else {
            bail!("compiler output cannot contain symlinks or special files");
        }
        if *files > 4096 || *bytes > 16 * 1024 * 1024 {
            bail!("compiler output exceeds 4096 entries / 16 MiB");
        }
    }
    Ok(())
}
