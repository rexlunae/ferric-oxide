#!/usr/bin/env bash
# Installs the build machine's programs that recipes need with the system
# package manager (Homebrew, apt or dnf), then runs `cargo xtask setup`.
#
#   ./native_bootstrap.sh                  install packages, then set up
#   ./native_bootstrap.sh --no-packages    only run cargo xtask setup
#
# Other arguments are passed to cargo xtask setup (for example --arch x86_64).
# The package lists here are the single source for the documented
# prerequisites; cargo xtask doctor checks the same programs.
set -euo pipefail
cd "$(dirname "$0")"

install_packages=1
setup_args=()
for arg in "$@"; do
    case "$arg" in
        --no-packages) install_packages=0 ;;
        *) setup_args+=("$arg") ;;
    esac
done

# Programs recipes run while building from source, OpenSSL 3 headers for the
# build-time Python (host:python312), NASM (x86-64 kernel)
# and QEMU with UEFI firmware (only to run the image).
debian_packages=(
    clang git curl tar make nasm
    autoconf automake autopoint bison cmake flex m4 texinfo meson ninja-build
    patch perl pkg-config python3 libssl-dev
    qemu-system-x86 qemu-system-arm qemu-efi-aarch64 ovmf
)
fedora_packages=(
    clang git curl tar make nasm
    autoconf automake gettext-devel bison cmake flex m4 texinfo meson ninja-build
    patch perl pkgconf-pkg-config python3 openssl-devel
    qemu-system-x86 qemu-system-aarch64 edk2-ovmf edk2-aarch64
)
# Xcode Command Line Tools supply Clang, Git, Make, Perl, patch and Python 3.
homebrew_packages=(
    nasm autoconf automake gettext bison flex texinfo cmake meson ninja pkg-config
    openssl@3 qemu
)
# Homebrew does not link these over macOS's older copies.
homebrew_keg_only=(bison flex texinfo gettext)

run() {
    echo "+ $*" >&2
    "$@"
}

install_host_packages() {
    case "$(uname -s)" in
        Darwin)
            if ! xcode-select -p >/dev/null 2>&1; then
                echo 'Install the Xcode Command Line Tools (xcode-select --install), then rerun this script.' >&2
                exit 1
            fi
            if ! command -v brew >/dev/null 2>&1; then
                echo 'Install Homebrew (https://brew.sh), or install the packages listed in this script another way and rerun it with --no-packages.' >&2
                exit 1
            fi
            run brew install "${homebrew_packages[@]}"
            local missing_path=()
            for formula in "${homebrew_keg_only[@]}"; do
                local bin
                bin="$(brew --prefix "$formula")/bin"
                case ":$PATH:" in
                    *":$bin:"*) ;;
                    *) missing_path+=("$bin") ;;
                esac
            done
            if [ "${#missing_path[@]}" -gt 0 ]; then
                local joined
                joined="$(IFS=:; echo "${missing_path[*]}")"
                echo "Add Homebrew's keg-only tools to your PATH ahead of the system copies, for example in ~/.zprofile:" >&2
                echo "  export PATH=\"$joined:\$PATH\"" >&2
                export PATH="$joined:$PATH"
            fi
            ;;
        Linux)
            if command -v apt-get >/dev/null 2>&1; then
                run sudo apt-get update
                run sudo apt-get install -y --no-install-recommends "${debian_packages[@]}"
            elif command -v dnf >/dev/null 2>&1; then
                run sudo dnf install -y "${fedora_packages[@]}"
            else
                echo 'No supported package manager (apt-get or dnf) found. Install the equivalents of the Debian packages listed in this script, then rerun it with --no-packages.' >&2
                exit 1
            fi
            ;;
        *)
            echo "Unsupported build machine $(uname -s); see docs/src/building-redox.md." >&2
            exit 1
            ;;
    esac
}

if [ "$install_packages" = 1 ]; then
    install_host_packages
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo 'Install Rust with rustup (https://rustup.rs), then rerun this script.' >&2
    exit 1
fi
# The guarded expansion keeps macOS bash 3.2 happy with an empty array under -u.
exec cargo xtask setup ${setup_args[@]+"${setup_args[@]}"}
