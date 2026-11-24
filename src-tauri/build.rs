#[cfg(not(feature = "desktop"))]
fn main() {}

#[cfg(feature = "desktop")]
fn main() {
    write_agent_assets();
    tauri_build::build();

    // Windows: Embed Common Controls v6 manifest for test binaries
    //
    // When running `cargo test`, the generated test executables don't include
    // the standard Tauri application manifest. Without Common Controls v6,
    // `tauri::test` calls fail with STATUS_ENTRYPOINT_NOT_FOUND.
    //
    // This workaround:
    // 1. Embeds the manifest into test binaries via /MANIFEST:EMBED
    // 2. Uses /MANIFEST:NO for the main binary to avoid duplicate resources
    //    (Tauri already handles manifest embedding for the app binary)
    #[cfg(target_os = "windows")]
    {
        let manifest_path = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"),
        )
        .join("common-controls.manifest");
        let manifest_arg = format!("/MANIFESTINPUT:{}", manifest_path.display());

        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg={}", manifest_arg);
        // Avoid duplicate manifest resources in binary builds.
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        println!("cargo:rerun-if-changed={}", manifest_path.display());
    }
}

#[cfg(feature = "desktop")]
fn write_agent_assets() {
    use std::{env, fs, path::PathBuf};

    const ASSET_DIR: &str = "CC_SWITCH_SSH_AGENT_ASSETS";
    println!("cargo:rerun-if-env-changed={ASSET_DIR}");
    let assets = env::var_os(ASSET_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/ssh-agent-assets")
        });
    let mut generated = String::new();
    for (name, platform) in [
        ("X86_64", "x86_64"),
        ("AARCH64", "aarch64"),
        ("DARWIN_ARM64", "darwin-arm64"),
    ] {
        let file = assets.join(format!("cc-switch-ssh-agent-{platform}"));
        println!("cargo:rerun-if-changed={}", file.display());
        if env::var("PROFILE").as_deref() == Ok("release") && !file.is_file() {
            panic!("Missing {}. Provide the bundled SSH agents in {ASSET_DIR} or build them with scripts/build-ssh-agent.sh.", file.display());
        }
        let contents = file.is_file().then(|| {
            let path = file;
            let path = fs::canonicalize(path).expect("Locate the SSH agent asset");
            format!("Some(include_bytes!({:?}))", path.to_string_lossy())
        });
        generated.push_str(&format!(
            "const {name}: Option<&[u8]> = {};\n",
            contents.as_deref().unwrap_or("None")
        ));
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ssh_agent_assets.rs");
    fs::write(output, generated).expect("Write the SSH asset declarations");
}
