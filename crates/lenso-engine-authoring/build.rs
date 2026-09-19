fn main() {
    println!(
        "cargo:rustc-env=LENSO_CLI_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
