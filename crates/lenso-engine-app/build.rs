fn main() {
    println!("cargo:rustc-check-cfg=cfg(generated_native_host)");
    fn assets(root: &std::path::Path, directory: &std::path::Path, output: &mut String) {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                assets(root, &path, output);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
                output.push_str(&format!(
                    "({:?}, include_bytes!({:?})),\n",
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                    path.to_str().unwrap()
                ));
            }
        }
    }
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("assets/terminal");
    for required in [
        "rust-sdk/Cargo.toml.template",
        "rust-sdk/src/lib.rs",
        "rust-macros/Cargo.toml.template",
        "rust-macros/src/lib.rs",
    ] {
        assert!(
            root.join(required).is_file(),
            "missing bundled terminal asset: {required}"
        );
    }
    println!("cargo:rerun-if-changed={}", root.display());
    let mut source = "const TERMINAL_ASSETS: &[(&str, &[u8])] = &[\n".to_owned();
    assets(&root, &root, &mut source);
    source.push_str("];\n");
    std::fs::write(
        std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("terminal_assets.rs"),
        source,
    )
    .unwrap();
    let target = std::env::var("TARGET").expect("Cargo always sets TARGET for build scripts");
    println!("cargo:rustc-env=LENSO_CLI_BUILD_TARGET={target}");
}
