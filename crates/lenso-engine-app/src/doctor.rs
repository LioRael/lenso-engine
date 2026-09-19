use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

use clap::Args;
use serde::Serialize;

use crate::plugins::{load_resolved_app, project_root};

#[derive(Clone, Debug, Args)]
pub struct DoctorArgs {
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Emit a stable JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

pub fn doctor(args: DoctorArgs) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    let mut checks = Vec::new();
    checks.push(path_check(
        "host_catalog",
        &root.join(".lenso/host-catalog.json"),
    ));
    checks.push(path_check("host_executable", &root.join(".lenso/host")));
    let resolution = load_resolved_app(&root);
    if let Ok(app) = &resolution {
        for tool in required_external_tools(
            app.plan()
                .plugin_instances()
                .iter()
                .map(|instance| instance.execution_class().as_str()),
        ) {
            let executable = match tool {
                "bun" => env::var_os("BUN_BIN").unwrap_or_else(|| "bun".into()),
                _ => tool.into(),
            };
            checks.push(command_check(tool, &executable, &["--version"]));
        }
    }
    checks.push(match resolution {
        Ok(app) => Check {
            name: "app_resolution",
            status: "passed",
            detail: format!(
                "{} Plugin Instance(s), {} Capability binding(s)",
                app.instances().len(),
                app.plan().capability_bindings().len()
            ),
        },
        Err(error) => Check {
            name: "app_resolution",
            status: "failed",
            detail: format!("{error:#}"),
        },
    });
    let failed = checks
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.doctor",
                "status": if failed == 0 { "passed" } else { "failed" },
                "root": root,
                "checks": checks,
            }))?
        );
    } else {
        for check in &checks {
            println!("{:<18} {:<7} {}", check.name, check.status, check.detail);
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} doctor check(s) failed");
    }
    Ok(())
}

fn required_external_tools<'a>(
    execution_classes: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'static str> {
    execution_classes
        .into_iter()
        .filter_map(|execution_class| match execution_class {
            // Bun is an external executable selected at runtime. Native Rust,
            // Process, Wasm, and QuickJS implementations are already hosted or
            // bundled, so Cargo and rustc are authoring tools rather than App
            // runtime prerequisites.
            "lenso.bun-process@1" => Some("bun"),
            _ => None,
        })
        .collect()
}

fn command_check(name: &'static str, executable: &OsStr, arguments: &[&str]) -> Check {
    match Command::new(executable).args(arguments).output() {
        Ok(output) if output.status.success() => Check {
            name,
            status: "passed",
            detail: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        },
        Ok(output) => Check {
            name,
            status: "failed",
            detail: format!("exited with {}", output.status),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Check {
            name,
            status: "failed",
            detail: format!(
                "{} is required by the resolved App execution classes but was not found",
                executable.to_string_lossy()
            ),
        },
        Err(error) => Check {
            name,
            status: "failed",
            detail: error.to_string(),
        },
    }
}

fn path_check(name: &'static str, path: &Path) -> Check {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_file() => Check {
            name,
            status: "passed",
            detail: path.display().to_string(),
        },
        Ok(_) => Check {
            name,
            status: "failed",
            detail: format!("{} is not a regular file", path.display()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && name == "host_executable" => {
            Check {
                name,
                status: "skipped",
                detail: format!("{} is required only by `lenso run`", path.display()),
            }
        }
        Err(error) => Check {
            name,
            status: "failed",
            detail: format!("{}: {error}", path.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bun_is_required_only_when_the_resolved_plan_selects_bun() {
        assert_eq!(
            required_external_tools(["lenso.bun-process@1"]),
            BTreeSet::from(["bun"])
        );
        assert!(
            required_external_tools([
                "lenso.native-rust@1",
                "lenso.process@1",
                "lenso.wasm-component@1",
                "lenso.quickjs@1",
            ])
            .is_empty()
        );
    }

    #[test]
    fn a_missing_selected_runtime_fails_closed() {
        let check = command_check(
            "bun",
            OsStr::new("/definitely-missing-lenso-doctor-runtime"),
            &["--version"],
        );

        assert_eq!(check.status, "failed");
        assert!(check.detail.contains("required by the resolved App"));
    }
}
