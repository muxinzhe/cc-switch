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
    if [[ $(uname -s) == Darwin ]]; then
        set -- "$@" darwin-arm64
    fi
fi
mkdir -p -- "$assets"
cargo_args=(--manifest-path "$project/src-tauri/Cargo.toml" --target-dir "$target_dir"
            --release --no-default-features --features ssh-agent --bin cc-switch-ssh-agent --locked)
export CARGO_PROFILE_RELEASE_LTO=false
for platform in "$@"; do
    case "$platform" in
        x86_64|aarch64)
            target="$platform-unknown-linux-musl"
            env_target=${target//-/_}
            upper_target=$(printf '%s' "$env_target" | tr '[:lower:]' '[:upper:]')
            env \
                "CC_SWITCH_AGENT_ZIG_TARGET=$platform-linux-musl" \
                "CARGO_TARGET_${upper_target}_LINKER=$toolchain/compiler" \
                "CC_$env_target=$toolchain/compiler" \
                "CXX_$env_target=$toolchain/cxx" \
                "AR_$env_target=$toolchain/ar" \
                cargo rustc "${cargo_args[@]}" --target "$target" -- -C link-self-contained=no
            output="$target_dir/$target/release/cc-switch-ssh-agent"
            ;;
        darwin-arm64)
            if [[ $(uname -s) != Darwin ]]; then
                printf 'Build %s on macOS with the Apple SDK.\n' "$platform" >&2; exit 1
            fi
            target=aarch64-apple-darwin
            if [[ $(rustc --print host-tuple) == "$target" ]]; then
                cargo build "${cargo_args[@]}"
                output="$target_dir/release/cc-switch-ssh-agent"
            else
                cargo build "${cargo_args[@]}" --target "$target"
                output="$target_dir/$target/release/cc-switch-ssh-agent"
            fi
            ;;
        *) printf 'Unsupported SSH agent platform: %s\n' "$platform" >&2; exit 1 ;;
    esac
    asset="$assets/cc-switch-ssh-agent-$platform"
    staged=$(mktemp "$assets/.build-XXXXXXXX")
    trap 'rm -f -- "$staged"' EXIT
    install -m 755 "$output" "$staged"
    if [[ "$platform" == darwin-arm64 ]]; then
        codesign --force --sign - --identifier com.ccswitch.ssh-agent "$staged"
        codesign --verify --strict "$staged"
    fi
    mv -f -- "$staged" "$asset"
    trap - EXIT
done
printf 'SSH agent assets are in %s\n' "$assets"
