#!/usr/bin/env bash
set -euo pipefail

project=$(cd "$(dirname "$0")/.." && pwd)
target_dir="$project/src-tauri/target"
assets=${CC_SWITCH_SSH_AGENT_ASSETS:-"$target_dir/ssh-agent-assets"}
toolchain="$target_dir/ssh-agent-zig"
mkdir -p -- "$toolchain"

cat > "$toolchain/compiler" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
driver=cc
if [[ "$0" == *cxx ]]; then driver=c++; fi
args=()
for argument in "$@"; do
    case "$argument" in
        --target=*-unknown-linux-musl) ;;
        *) args+=("$argument") ;;
    esac
done
exec zig "$driver" -target "$CC_SWITCH_AGENT_ZIG_TARGET" "${args[@]}"
EOF
cat > "$toolchain/ar" <<'EOF'
#!/usr/bin/env bash
exec zig ar "$@"
EOF
chmod 700 "$toolchain/compiler" "$toolchain/ar"
ln -sf "$toolchain/compiler" "$toolchain/cxx"

if [[ $# -eq 0 ]]; then
    set -- x86_64 aarch64
fi
mkdir -p -- "$assets"
for architecture in "$@"; do
    case "$architecture" in
        x86_64|aarch64) ;;
        *) printf 'Unsupported SSH agent architecture: %s\n' "$architecture" >&2; exit 1 ;;
    esac
    target="$architecture-unknown-linux-musl"
    env_target=${target//-/_}
    upper_target=$(printf '%s' "$env_target" | tr '[:lower:]' '[:upper:]')
    env         "CC_SWITCH_AGENT_ZIG_TARGET=$architecture-linux-musl"         "CARGO_TARGET_${upper_target}_LINKER=$toolchain/compiler"         "CC_$env_target=$toolchain/compiler"         "CXX_$env_target=$toolchain/cxx"         "AR_$env_target=$toolchain/ar"         CARGO_PROFILE_RELEASE_LTO=false         cargo rustc --manifest-path "$project/src-tauri/Cargo.toml" --target-dir "$target_dir"             --release --target "$target" --no-default-features --features ssh-agent             --bin cc-switch-ssh-agent --locked -- -C link-self-contained=no
    cp "$target_dir/$target/release/cc-switch-ssh-agent" "$assets/cc-switch-ssh-agent-$architecture"
done
printf 'SSH agent assets are in %s\n' "$assets"
