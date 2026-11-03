// Scripts here is executed using "cookbook_redoxer env" where CC, RUSTFLAGS, etc. defined.
// Look up redoxer env script if you want to see how they work.

pub(crate) static SHARED_PRESCRIPT: &str = concat!(
    r#"
# Build dynamically
function DYNAMIC_INIT {
    case "${TARGET}" in
        "riscv64gc-unknown-redox")
            [ -z "${COOKBOOK_VERBOSE}" ] || echo "WARN: ${TARGET} does not support dynamic linking." >&2
            return
            ;;
    esac

    [ -z "${COOKBOOK_VERBOSE}" ] || echo "DEBUG: Program is being compiled dynamically."

    COOKBOOK_CONFIGURE_FLAGS=(
        --host="${GNU_TARGET}"
        --prefix="/usr"
        --enable-shared
        --disable-static
    )

    COOKBOOK_CMAKE_FLAGS=(
        -DBUILD_SHARED_LIBS=True
        -DENABLE_SHARED=True
        -DENABLE_STATIC=False
    )

    COOKBOOK_MESON_FLAGS=(
        --buildtype release
        --wrap-mode nofallback
        -Ddefault_library=shared
        -Dprefix=/usr
    )

    # TODO: check paths for spaces
    case "${TARGET}" in
        *-apple-darwin)
            # host: recipes on macOS link with ld64, which has no
            # -rpath-link or --export-dynamic; those are for ELF targets.
            export LDFLAGS="${USER_LDFLAGS}-L${COOKBOOK_SYSROOT}/lib"
            export RUSTFLAGS="-L native=${COOKBOOK_SYSROOT}/lib"
            ;;
        *)
            export LDFLAGS="${USER_LDFLAGS}-Wl,-rpath-link,${COOKBOOK_SYSROOT}/lib -L${COOKBOOK_SYSROOT}/lib -Wl,--export-dynamic"
            export RUSTFLAGS="-C target-feature=-crt-static -L native=${COOKBOOK_SYSROOT}/lib -C link-arg=-Wl,-rpath-link,${COOKBOOK_SYSROOT}/lib"
            ;;
    esac
    export COOKBOOK_DYNAMIC=1

    if [ "function" = "$(type -t ferric_target_rustflags)" ]; then
        ferric_target_rustflags
    fi

    if [ "function" = "$(type -t reexport_flags)" ]; then
        reexport_flags
    fi
}

COOKBOOK_AUTORECONF="autoreconf"
autotools_recursive_regenerate() {
    for f in $(find . -name configure.ac -o -name configure.in -type f | sort); do
        echo "* autotools regen in '$(dirname $f)'..."
        ( cd "$(dirname "$f")" && "${COOKBOOK_AUTORECONF}" -fvi "$@" -I${COOKBOOK_LIBTOOL_DIR:-$COOKBOOK_HOST_SYSROOT}/share/aclocal )
    done
}

# Build both dynamically and statically
function DYNAMIC_STATIC_INIT {
    DYNAMIC_INIT
    if [ "${COOKBOOK_DYNAMIC}" == "1" ]
    then
        COOKBOOK_CONFIGURE_FLAGS=(
            --host="${GNU_TARGET}"
            --prefix="/usr"
            --enable-shared
            --enable-static
        )

        COOKBOOK_CMAKE_FLAGS=(
            -DBUILD_SHARED_LIBS=True
            -DENABLE_SHARED=True
            -DENABLE_STATIC=True
        )

        COOKBOOK_MESON_FLAGS=(
            --buildtype release
            --wrap-mode nofallback
            -Ddefault_library=both
            -Dprefix=/usr
        )
    fi
}

# Prints the first prefix among "$@" whose include/openssl/opensslv.h is
# OpenSSL 3, for host: builds that must not pick up OpenSSL 4 (CPython 3.12,
# for one, does not build against it). Returns 1 when none is.
function HOST_OPENSSL3_PREFIX {
    local candidate
    for candidate in "$@"; do
        if [ -n "${candidate}" ] && grep -Eq '^#[[:space:]]*define[[:space:]]+OPENSSL_VERSION_MAJOR[[:space:]]+3[[:space:]]*$' \
            "${candidate}/include/openssl/opensslv.h" 2>/dev/null; then
            echo "${candidate}"
            return 0
        fi
    done
    return 1
}

# Writes GNU config.sub (vendored in tools/cookbook/res, upstream GNU config 2022-01-03) to $1.
function GNU_CONFIG_GET {
  cat > "$1" <<'FERRIC_CONFIG_SUB_EOF'
"#,
    include_str!("../../res/config.sub"),
    r#"FERRIC_CONFIG_SUB_EOF
  chmod +x "$1"
}
"#
);

pub(crate) static BUILD_PRESCRIPT: &str = r#"
# redoxer exports the strip it selected for host builds as STRIP (unprefixed
# strip, llvm-strip with Clang, or REDOXER_HOST_STRIP). Recipe scripts may unset
# STRIP (cookbook_meson does), so keep it for package stripping in BUILD_POSTSCRIPT.
COOKBOOK_HOST_STRIP="${STRIP-}"

# Add cookbook bins to path
export PATH="${COOKBOOK_ROOT}/bin:${PATH}"

# Add toolchain dir to path if exists. It holds the build tools built for the
# host.
if [ ! -z "${COOKBOOK_TOOLCHAIN}" ]
then
export PATH="${COOKBOOK_TOOLCHAIN}/bin:${PATH}"
export LD_LIBRARY_PATH="${COOKBOOK_TOOLCHAIN}/lib:${LD_LIBRARY_PATH}"
fi

# With build.build_tools = "guest", the tools built for the target are Redox
# binaries the host cannot run. Each gets a shim of the same name, and the shim
# runs the real tool in the guest build environment, so a recipe keeps calling
# the tool by name.
if [ -n "${COOKBOOK_GUEST_TOOLS_SHIMS}" ]
then
export PATH="${COOKBOOK_GUEST_TOOLS_SHIMS}:${PATH}"
fi

# This puts cargo build artifacts in the build directory
export CARGO_TARGET_DIR="${COOKBOOK_BUILD}/target"

# This adds the sysroot includes for most C compilation
#TODO: check paths for spaces!
export CPPFLAGS="${CPPFLAGS:+$CPPFLAGS }-I${COOKBOOK_SYSROOT}/include"

# This adds the sysroot libraries and compiles binaries statically for most C compilation
#TODO: check paths for spaces!
USER_LDFLAGS="${LDFLAGS:+$LDFLAGS }"
export LDFLAGS="${USER_LDFLAGS}-L${COOKBOOK_SYSROOT}/lib --static"

# This reexport C variables into custom build script that can be consumed by cc crate
function reexport_flags {
    target=${TARGET//-/_}
    export CFLAGS_${target}="${CFLAGS:+$CFLAGS }${CPPFLAGS}"
    export CXXFLAGS_${target}="${CXXFLAGS:+$CXXFLAGS }${CPPFLAGS}"
    export LDFLAGS_${target}="${LDFLAGS}"
}

# These ensure that pkg-config gets the right flags from the sysroot
if [ "${TARGET}" != "${COOKBOOK_HOST_TARGET}" ]
then
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_PATH=
    export PKG_CONFIG_LIBDIR="${COOKBOOK_SYSROOT}/lib/pkgconfig"
    export PKG_CONFIG_SYSROOT_DIR="${COOKBOOK_SYSROOT}"
fi

# To build the debug version of a Cargo program, add COOKBOOK_DEBUG=true, and
# to not strip symbols from the final package, add COOKBOOK_NOSTRIP=true to the recipe
# (or to your environment) before calling cookbook_cargo or cookbook_cargo_packages
build_type=release
install_flags=--no-track
build_flags=--release
if [ ! -z "${COOKBOOK_DEBUG}" ]
then
    install_flags+=" --debug"
    build_flags=
    build_type=debug
    export CPPFLAGS="${CPPFLAGS} -g"
fi

# Ferric Oxide: COOKBOOK_DEBUGINFO=true (build.debug in ferric.toml) keeps the optimized
# release build and adds full debug info to it, so debugging a local image does not also
# make it run unoptimized.
if [ ! -z "${COOKBOOK_DEBUGINFO}" ]
then
    export CARGO_PROFILE_RELEASE_DEBUG=true
    export CARGO_PROFILE_RELEASE_STRIP=false
    export CPPFLAGS="${CPPFLAGS} -g"
fi

if [ ! -z "${COOKBOOK_OFFLINE}" ]
then
build_flags+=" --offline"
install_flags+=" --offline"
fi

# Ferric Oxide: processor baseline and extra target flags from ferric.toml
# (make menuconfig). Only packages built for the target get them, and never
# the recipes in FERRIC_TARGET_FLAGS_EXCLUDE (the kernel and bootloader).
function ferric_target_flags_apply {
    [ "${TARGET}" != "${COOKBOOK_HOST_TARGET}" ] || return 1
    case " ${FERRIC_TARGET_FLAGS_EXCLUDE} " in
        *" ${COOKBOOK_NAME} "*) return 1 ;;
    esac
}

function ferric_target_rustflags {
    [ -n "${FERRIC_TARGET_RUSTFLAGS}" ] && ferric_target_flags_apply || return 0
    # redoxer moves RUSTFLAGS into the per-target variable it names here
    if [ -n "${RUSTFLAGS}" ]
    then
        export RUSTFLAGS="${RUSTFLAGS} ${FERRIC_TARGET_RUSTFLAGS}"
    elif [ -n "${REDOXER_RUSTFLAGS_KEY}" ]
    then
        export "${REDOXER_RUSTFLAGS_KEY}=${!REDOXER_RUSTFLAGS_KEY:+${!REDOXER_RUSTFLAGS_KEY} }${FERRIC_TARGET_RUSTFLAGS}"
    else
        export RUSTFLAGS="${FERRIC_TARGET_RUSTFLAGS}"
    fi
}

ferric_target_rustflags
if [ -n "${FERRIC_TARGET_CPPFLAGS}" ] && ferric_target_flags_apply
then
    export CPPFLAGS="${CPPFLAGS} ${FERRIC_TARGET_CPPFLAGS}"
fi

reexport_flags

COOKBOOK_CARGO="${COOKBOOK_REDOXER}"
COOKBOOK_CARGO_FLAGS=(
    --locked
)
# cargo template using cargo install
function cookbook_cargo {
    "${COOKBOOK_CARGO}" install \
        --path "${COOKBOOK_SOURCE}${COOKBOOK_CARGO_PATH:+/$COOKBOOK_CARGO_PATH}" \
        --root "${COOKBOOK_STAGE}/usr" \
        -j "${COOKBOOK_MAKE_JOBS}" ${install_flags} \
        ${COOKBOOK_CARGO_FLAGS[@]} "$@"
}

# cargo template using cargo build (prefixed name)
function cookbook_cargo_build {
    recipe="${recipe:-$(basename "${COOKBOOK_RECIPE}")}"
    bin_dir="${bin_dir:-.}"
    bin_flags="${bin_flags:-}"
    bin_name="${bin_name:-$(basename "${COOKBOOK_CARGO_PATH}")}"
    bin_final_name="${bin_final_name:-${recipe}_${bin_name//_/-}}"
    mkdir -pv "${COOKBOOK_STAGE}/usr/bin"
    "${COOKBOOK_CARGO}" build \
        --manifest-path "${COOKBOOK_SOURCE}${COOKBOOK_CARGO_PATH:+/$COOKBOOK_CARGO_PATH}/Cargo.toml" \
        ${bin_flags} ${build_flags} -j "${COOKBOOK_MAKE_JOBS}" ${COOKBOOK_CARGO_FLAGS[@]}
    cp -v \
        "target/${TARGET}/${build_type}/${bin_dir}/${bin_name}" \
        "${COOKBOOK_STAGE}/usr/bin/${bin_final_name}"
    unset bin_name bin_flags bin_dir bin_final_name
}

# helper for installing binaries that are cargo examples
function cookbook_cargo_examples {
    recipe="$(basename "${COOKBOOK_RECIPE}")"
    for example in "$@"
    do
        bin_dir="examples" bin_name="${example}" bin_flags="--example ${example}" cookbook_cargo_build
    done
}

# helper for installing binaries that are cargo packages
function cookbook_cargo_packages {
    recipe="$(basename "${COOKBOOK_RECIPE}")"
    mkdir -pv "${COOKBOOK_STAGE}/usr/bin"
    for package in "$@"
    do
        bin_name="${package}" bin_flags="--package ${package}" bin_final_name="${package//_/-}" cookbook_cargo_build
    done
}

# helper for installing binaries that are cargo packages that are prefixed
function cookbook_cargo_packages_prefixed {
    recipe="$(basename "${COOKBOOK_RECIPE}")"
    mkdir -pv "${COOKBOOK_STAGE}/usr/bin"
    for package in "$@"
    do
        bin_name="${package}" bin_flags="--package ${package}" cookbook_cargo_build
    done
}

# configure template
COOKBOOK_CONFIGURE="${COOKBOOK_SOURCE}/configure"
COOKBOOK_CONFIGURE_FLAGS=(
    --host="${GNU_TARGET}"
    --prefix="/usr"
    --disable-shared
    --enable-static
)
COOKBOOK_MAKE_FLAGS=()
COOKBOOK_MAKE="make"

function cookbook_configure {
    "${COOKBOOK_CONFIGURE}" "${COOKBOOK_CONFIGURE_FLAGS[@]}" "$@"
    "${COOKBOOK_MAKE}" -j "${COOKBOOK_MAKE_JOBS}" "${COOKBOOK_MAKE_FLAGS[@]}"
    "${COOKBOOK_MAKE}" install DESTDIR="${COOKBOOK_STAGE}"
}

COOKBOOK_CMAKE="cmake"
COOKBOOK_NINJA="ninja"
COOKBOOK_CMAKE_FLAGS=(
    -DBUILD_SHARED_LIBS=False
    -DENABLE_SHARED=False
    -DENABLE_STATIC=True
)

function generate_cookbook_cmake_file {
    target=$1
    gcc_prefix=$2
    sysroot=$3
    file=$4
    ARCH=$(echo "$target" | cut -d - -f1)

    if [[ "$target" == *"-linux-"* ]]; then
        SYSTEM_NAME="Linux"
    else
        SYSTEM_NAME="UnixPaths"
    fi

if [[ "$REDOXER_USE_CLANG" != "1" && "$REDOXER_USE_CLANG" != "true" ]]; then
cat > $file <<EOF
set(CMAKE_AR ${gcc_prefix}gcc-ar)
set(CMAKE_CXX_COMPILER ${gcc_prefix}g++)
set(CMAKE_C_COMPILER ${gcc_prefix}gcc)
set(CMAKE_NM ${gcc_prefix}gcc-nm)
set(CMAKE_RANLIB ${gcc_prefix}gcc-ranlib)
EOF
else
cat > $file <<EOF
set(CMAKE_AR llvm-ar)
set(CMAKE_CXX_COMPILER clang++)
set(CMAKE_C_COMPILER clang)
set(CMAKE_NM llvm-nm)
set(CMAKE_RANLIB llvm-ranlib)
EOF

if [ "$target" != "$COOKBOOK_HOST_TARGET" ]; then
cat >> $file <<EOF
set(CMAKE_C_COMPILER_TARGET ${target})
set(CMAKE_CXX_COMPILER_TARGET ${target})
set(CMAKE_ASM_COMPILER_TARGET ${target})
EOF
# redox clang workaround (see redoxer env)
CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-stdlib=libstdc++"
fi

fi
    cat >> $file <<EOF
set(CMAKE_FIND_ROOT_PATH ${sysroot})
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_PLATFORM_USES_PATH_WHEN_NO_SONAME 1)
set(CMAKE_PREFIX_PATH, ${sysroot})
set(CMAKE_SHARED_LIBRARY_SONAME_C_FLAG "-Wl,-soname,")
set(CMAKE_SYSTEM_NAME ${SYSTEM_NAME})
set(CMAKE_SYSTEM_PROCESSOR ${ARCH})
EOF

    if [ "$target" = "$GNU_TARGET" ]
    then
        echo "set(CMAKE_C_FLAGS \"${CFLAGS} ${CPPFLAGS}\")" >> $file
        echo "set(CMAKE_CXX_FLAGS \"${CXXFLAGS} ${CPPFLAGS}\")" >> $file
    fi

    if [ -n "${CC_WRAPPER}" ]
    then
        echo "set(CMAKE_C_COMPILER_LAUNCHER ${CC_WRAPPER})" >> $file
        echo "set(CMAKE_CXX_COMPILER_LAUNCHER ${CC_WRAPPER})" >> $file
    fi
}

function cookbook_cmake {

    if [ "$TARGET" = "$COOKBOOK_HOST_TARGET" ]; then
        GCC_PREFIX=
    else
        GCC_PREFIX=$GNU_TARGET-
    fi
    generate_cookbook_cmake_file "$GNU_TARGET" "$GCC_PREFIX" "$COOKBOOK_SYSROOT" cross_file.cmake

    "${COOKBOOK_CMAKE}" "${COOKBOOK_SOURCE}" \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_CROSSCOMPILING=True \
        -DCMAKE_INSTALL_INCLUDEDIR=include \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DCMAKE_INSTALL_OLDINCLUDEDIR=/include \
        -DCMAKE_INSTALL_PREFIX=/usr \
        -DCMAKE_INSTALL_SBINDIR=bin \
        -DCMAKE_TOOLCHAIN_FILE=cross_file.cmake \
        -GNinja \
        -Wno-dev \
        "${COOKBOOK_CMAKE_FLAGS[@]}" \
        "$@"

    "${COOKBOOK_NINJA}" -j"${COOKBOOK_MAKE_JOBS}"
    DESTDIR="${COOKBOOK_STAGE}" "${COOKBOOK_NINJA}" install -j"${COOKBOOK_MAKE_JOBS}"
}

COOKBOOK_MESON="meson"
COOKBOOK_MESON_FLAGS=(
    --buildtype release
    --wrap-mode nofallback
    -Ddefault_library=static
    -Dprefix=/usr
)
function cookbook_meson {
    # TODO: do this in rust, to handle path spaces as well
    function format_flags {
        local flags=($1)
        local formatted=""
        for i in "${!flags[@]}"; do
            formatted+="'${flags[$i]}'"
            if [ $i -lt $((${#flags[@]} - 1)) ]; then
                formatted+=", "
            fi
        done
        echo "$formatted"
    }

    echo "[binaries]" > cross_file.txt
    echo "c = [$(printf "'%s', " $CC | sed 's/, $//')]"  >> cross_file.txt
    echo "cpp = [$(printf "'%s', " $CXX | sed 's/, $//')]" >> cross_file.txt
    echo "ar = '${AR}'" >> cross_file.txt
    echo "strip = '${STRIP}'" >> cross_file.txt
    echo "pkg-config = '${PKG_CONFIG}'" >> cross_file.txt
    echo "llvm-config = '${TARGET}-llvm-config'" >> cross_file.txt
    echo "glib-compile-resources = 'glib-compile-resources'" >> cross_file.txt
    echo "glib-compile-schemas = 'glib-compile-schemas'" >> cross_file.txt

    echo "[host_machine]" >> cross_file.txt
    echo "system = '$(echo "${TARGET}" | cut -d - -f3)'" >> cross_file.txt
    echo "cpu_family = '$(echo "${TARGET}" | cut -d - -f1)'" >> cross_file.txt
    echo "cpu = '$(echo "${TARGET}" | cut -d - -f1)'" >> cross_file.txt
    echo "endian = 'little'" >> cross_file.txt

    echo "[built-in options]" >> cross_file.txt
    echo "prefix = '/usr'" >> cross_file.txt
    echo "libdir = 'lib'" >> cross_file.txt
    echo "bindir = 'bin'" >> cross_file.txt
    echo "c_args = [$(format_flags "$CFLAGS $CPPFLAGS")]" >> cross_file.txt
    echo "cpp_args = [$(format_flags "$CXXFLAGS $CPPFLAGS")]" >> cross_file.txt
    echo "c_link_args = [$(format_flags "$LDFLAGS")]" >> cross_file.txt

    echo "[properties]" >> cross_file.txt
    echo "needs_exe_wrapper = true" >> cross_file.txt
    echo "sys_root = '${COOKBOOK_SYSROOT}'" >> cross_file.txt

    unset AR AS CC CXX LD NM OBJCOPY OBJDUMP PKG_CONFIG RANLIB READELF STRIP

    "${COOKBOOK_MESON}" setup \
        "${COOKBOOK_SOURCE}" \
        . \
        --cross-file cross_file.txt \
        "${COOKBOOK_MESON_FLAGS[@]}" \
        "$@"
    "${COOKBOOK_NINJA}" -j"${COOKBOOK_MAKE_JOBS}"
    DESTDIR="${COOKBOOK_STAGE}" "${COOKBOOK_NINJA}" install -j"${COOKBOOK_MAKE_JOBS}"
}
COOKBOOK_PYTHON="${COOKBOOK_TOOLCHAIN}/bin/python3"
COOKBOOK_PYTHON_V="python3.12"
function PYTHON_INIT {
    ARCH="${TARGET%%-*}"
    OS=$(echo "${TARGET}" | cut -d - -f3-4)
    SYSTEM=$(echo "${TARGET}" | cut -d - -f3)
    export PYTHONPYCACHEPREFIX="${COOKBOOK_BUILD}" _PYTHON_HOST_PLATFORM="$OS-$ARCH"
    # Build modules execute in the host interpreter. Only the target's pure
    # sysconfig data belongs on its search path, not target extension modules.
    export PYTHONPATH="${COOKBOOK_TOOLCHAIN}/lib/${COOKBOOK_PYTHON_V}:${COOKBOOK_TOOLCHAIN}/lib/${COOKBOOK_PYTHON_V}/site-packages:${COOKBOOK_SYSROOT}/usr/lib/${COOKBOOK_PYTHON_V}${PYTHONPATH:+:$PYTHONPATH}"
    export _PYTHON_SYSCONFIGDATA_NAME="_sysconfigdata__${SYSTEM}_${ARCH}-${OS}"
}
COOKBOOK_PYTHON_PIP_FLAGS=(
    --ignore-installed --no-index --no-build-isolation
)
function cookbook_python {
    PYTHON_INIT
    # no way to do out-of-tree build: https://github.com/pypa/setuptools/issues/3237
    rsync -a "${COOKBOOK_SOURCE}/" ./source
    if [ -n "${COOKBOOK_PYTHON_LEGACY_SETUP}" ]; then
        pushd ./source
        python3 ./setup.py install
        popd
    fi
    "${COOKBOOK_PYTHON}" -m pip install --prefix="${COOKBOOK_STAGE}/usr" \
    "./source${COOKBOOK_PYTHON_PYPROJECT_PATH:+/$COOKBOOK_PYTHON_PYPROJECT_PATH}" \
    "${COOKBOOK_PYTHON_PIP_FLAGS[@]}" "$@"
    rsync -av "${COOKBOOK_BUILD}/${COOKBOOK_STAGE}/usr/" "${COOKBOOK_STAGE}/usr"
}
"#;

pub(crate) static BUILD_POSTSCRIPT: &str = r#"
# Runs strip with the given options on every ELF file and static archive under a
# directory. Cross builds use "${GNU_TARGET}-strip"; host: builds use the strip
# redoxer selected for the host (COOKBOOK_HOST_STRIP, see BUILD_PRESCRIPT). The
# options must be ones both GNU strip and llvm-strip accept. Other files
# (scripts, linker scripts, pkg-config and data files) are not passed to strip.
# A strip failure on an object, or a failure to list the directory, is reported
# and fails the build once the whole directory has been processed. host: recipes
# on macOS produce Mach-O files, so their packages are not stripped.
function COOKBOOK_STRIP_OBJECTS {
    case "${TARGET}" in
        *-apple-darwin) return 0 ;;
    esac
    local dir="$1"
    shift
    local strip="${GNU_TARGET}-strip"
    if [ "${TARGET}" = "${COOKBOOK_HOST_TARGET}" ]
    then
        strip="${COOKBOOK_HOST_STRIP:-strip}"
    fi
    # find runs in a process substitution, whose exit status bash discards, so
    # it appends its status as a final record.
    local records=() record
    while IFS= read -r -d '' record
    do
        records+=("${record}")
    done < <(
        if find "${dir}" -type f -print0
        then
            printf '0\0'
        else
            printf '%d\0' "$?"
        fi
    )
    local count="${#records[@]}" failed=0
    if [ "${count}" -eq 0 ]
    then
        echo "error: could not list the files to strip in ${dir}" >&2
        return 1
    fi
    if [ "${records[count - 1]}" != 0 ]
    then
        echo "error: could not list all the files to strip in ${dir}" >&2
        failed=1
    fi
    local file magic
    for file in "${records[@]:0:count - 1}"
    do
        magic="$(od -An -tx1 -N8 -- "${file}")"
        magic="${magic//[[:space:]]/}"
        case "${magic}" in
            # ELF ("\x7fELF") or a regular ar archive ("!<arch>\n")
            7f454c46*|213c617263683e0a) ;;
            *) continue ;;
        esac
        if "${strip}" "$@" -- "${file}"
        then
            echo "stripped ${file}"
        else
            echo "error: ${strip} $* failed on ${file}" >&2
            failed=1
        fi
    done
    return "${failed}"
}

# Strip binaries
for dir in "${COOKBOOK_STAGE}/bin" "${COOKBOOK_STAGE}/usr/bin" "${COOKBOOK_STAGE}/libexec" "${COOKBOOK_STAGE}/usr/libexec"
do
    if [ -d "${dir}" ] && [ -z "${COOKBOOK_NOSTRIP}" ]
    then
        COOKBOOK_STRIP_OBJECTS "${dir}" --strip-all
    fi
done

# Remove libtool files and strip debugging information
for dir in "${COOKBOOK_STAGE}/lib" "${COOKBOOK_STAGE}/usr/lib"
do
    if [ -d "${dir}" ]
    then
        find "${dir}" -type f -name '*.la' -exec rm -fv {} ';'
        if [ -z "${COOKBOOK_NOSTRIP}" ]
        then
            COOKBOOK_STRIP_OBJECTS "${dir}" --strip-debug
        fi
    fi
done

# Remove cargo install files
for file in .crates.toml .crates2.json
do
    if [ -f "${COOKBOOK_STAGE}/${file}" ]
    then
        rm -v "${COOKBOOK_STAGE}/${file}"
    fi
done

# Add pkgname to appstream metadata
for dir in "${COOKBOOK_STAGE}/share/metainfo" "${COOKBOOK_STAGE}/usr/share/metainfo"
do
    if [ -d "${dir}" ]
    then
        find "${dir}" -type f -name '*.xml' -exec sed -i 's|</component>|<pkgname>'"${COOKBOOK_NAME}"'</pkgname></component>|g' {} ';'
    fi
done
"#;

pub static KILL_ALL_PID: &str = r#"
THISPID=$$
CHILDREN=$(ps -o pid= --ppid $PID | grep -v $THISPID);

ALL_DESCENDANTS='';

while [ -n "$CHILDREN" ]; do
    ALL_DESCENDANTS="$ALL_DESCENDANTS $CHILDREN";
    CHILDREN=$(ps -o pid= --ppid $(echo $CHILDREN) | tr '\n' ' ');
done;

if [ -n "$ALL_DESCENDANTS" ]; then
    kill $ALL_DESCENDANTS;
fi

killall sccache
"#;

#[cfg(test)]
mod tests {
    use super::{BUILD_POSTSCRIPT, BUILD_PRESCRIPT, SHARED_PRESCRIPT};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    /// Runs the build prescript with `env` and returns the Cargo flags and release-profile
    /// overrides it leaves for `cookbook_cargo`.
    fn cargo_settings(env: &[(&str, &str)]) -> String {
        let script = format!(
            "{BUILD_PRESCRIPT}\nprintf '%s|%s|%s|%s' \"$build_flags\" \"$build_type\" \
             \"${{CARGO_PROFILE_RELEASE_DEBUG-}}\" \"${{CARGO_PROFILE_RELEASE_STRIP-}}\"\n"
        );
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new("bash")
            .arg("-e")
            .arg("-c")
            .arg(script)
            .current_dir(dir.path())
            .env_remove("COOKBOOK_DEBUG")
            .env_remove("COOKBOOK_DEBUGINFO")
            .env_remove("COOKBOOK_OFFLINE")
            .env_remove("CARGO_PROFILE_RELEASE_DEBUG")
            .env_remove("CARGO_PROFILE_RELEASE_STRIP")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn debug_info_keeps_the_release_build() {
        assert_eq!(cargo_settings(&[]), "--release|release||");
        assert_eq!(
            cargo_settings(&[("COOKBOOK_DEBUGINFO", "true")]),
            "--release|release|true|false"
        );
    }

    #[test]
    fn unoptimized_builds_drop_release() {
        assert_eq!(cargo_settings(&[("COOKBOOK_DEBUG", "true")]), "|debug||");
        // With both set the debug profile wins; the release overrides go unused.
        assert_eq!(
            cargo_settings(&[("COOKBOOK_DEBUG", "true"), ("COOKBOOK_DEBUGINFO", "true")]),
            "|debug|true|false"
        );
    }

    #[test]
    fn gnu_config_get_writes_the_vendored_config_sub() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "{SHARED_PRESCRIPT}\nGNU_CONFIG_GET config.sub\n./config.sub x86_64-unknown-redox\n"
        );
        let output = Command::new("bash")
            .arg("-e")
            .current_dir(dir.path())
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"x86_64-unknown-redox\n");
        assert_eq!(
            std::fs::read(dir.path().join("config.sub")).unwrap(),
            include_bytes!("../../res/config.sub")
        );
    }

    /// Runs `DYNAMIC_INIT` for `target` and returns the linker flags it exports.
    fn dynamic_link_flags(target: &str) -> String {
        let script = format!(
            "{SHARED_PRESCRIPT}\nDYNAMIC_INIT\nprintf '%s|%s' \"$LDFLAGS\" \"$RUSTFLAGS\"\n"
        );
        let output = Command::new("bash")
            .arg("-e")
            .arg("-c")
            .arg(script)
            .env("TARGET", target)
            .env("COOKBOOK_SYSROOT", "/sysroot")
            .env_remove("USER_LDFLAGS")
            .env_remove("COOKBOOK_VERBOSE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn dynamic_init_uses_elf_linker_flags_only_for_elf_targets() {
        let redox = dynamic_link_flags("aarch64-unknown-redox");
        assert!(redox.contains("-Wl,-rpath-link,/sysroot/lib"), "{redox}");
        assert!(redox.contains("-Wl,--export-dynamic"), "{redox}");

        let macos = dynamic_link_flags("aarch64-apple-darwin");
        assert_eq!(macos, "-L/sysroot/lib|-L native=/sysroot/lib");
    }

    /// Runs `HOST_OPENSSL3_PREFIX` over `candidates` and returns its output, or
    /// `None` when it reports that none is OpenSSL 3.
    fn host_openssl3_prefix(candidates: &[&std::path::Path]) -> Option<String> {
        let output = Command::new("bash")
            .arg("-e")
            .arg("-c")
            .arg(format!("{SHARED_PRESCRIPT}\nHOST_OPENSSL3_PREFIX \"$@\""))
            .arg("bash")
            .arg("")
            .args(candidates)
            .output()
            .unwrap();
        output.status.success().then(|| {
            String::from_utf8(output.stdout)
                .unwrap()
                .trim_end()
                .to_owned()
        })
    }

    fn openssl_prefix(root: &std::path::Path, name: &str, major: u32) -> std::path::PathBuf {
        let prefix = root.join(name);
        std::fs::create_dir_all(prefix.join("include/openssl")).unwrap();
        std::fs::write(
            prefix.join("include/openssl/opensslv.h"),
            format!("# define OPENSSL_VERSION_MAJOR  {major}\n# define OPENSSL_VERSION_MINOR  0\n"),
        )
        .unwrap();
        prefix
    }

    #[test]
    fn host_openssl3_prefix_skips_other_versions_and_missing_headers() {
        let root = tempfile::tempdir().unwrap();
        let v4 = openssl_prefix(root.path(), "v4", 4);
        let v30 = openssl_prefix(root.path(), "v30", 30);
        let v3 = openssl_prefix(root.path(), "v3", 3);
        let missing = root.path().join("missing");

        assert_eq!(
            host_openssl3_prefix(&[&v4, &missing, &v30, &v3]),
            Some(v3.display().to_string())
        );
        assert_eq!(host_openssl3_prefix(&[&v4, &missing]), None);
    }

    const STRIP_TARGET: &str = "x86_64-unknown-redox";

    /// Installs `tool` as `${GNU_TARGET}-strip` in a fresh `bin` directory under `root`.
    fn strip_bin_dir(root: &Path, tool: &Path) -> PathBuf {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(tool, bin.join(format!("{STRIP_TARGET}-strip"))).unwrap();
        bin
    }

    /// Runs the build postscript on `stage` for a cross build, with `strip_bin`
    /// first in `PATH`.
    fn run_postscript(stage: &Path, strip_bin: &Path) -> Output {
        run_strip_script(
            BUILD_POSTSCRIPT,
            stage,
            strip_bin,
            &[("TARGET", STRIP_TARGET), ("GNU_TARGET", STRIP_TARGET)],
        )
    }

    /// Runs `script` on `stage` with `strip_bin` first in `PATH` and `env` set.
    fn run_strip_script(
        script: &str,
        stage: &Path,
        strip_bin: &Path,
        env: &[(&str, &str)],
    ) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![strip_bin.to_owned()];
        paths.extend(std::env::split_paths(&path));
        Command::new("bash")
            .arg("-e")
            .arg("-c")
            .arg(script)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("COOKBOOK_STAGE", stage)
            .env("COOKBOOK_NAME", "strip-test")
            .env_remove("COOKBOOK_NOSTRIP")
            .env_remove("COOKBOOK_HOST_TARGET")
            .env_remove("STRIP")
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    /// Returns the sorted strip invocations in `log`, with `stage` removed from paths.
    fn strip_calls(log: &Path, stage: &Path) -> Vec<String> {
        let mut calls: Vec<String> = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|line| line.replace(&stage.display().to_string(), ""))
            .collect();
        calls.sort();
        calls
    }

    const EXPECTED_STRIP_CALLS: [&str; 3] = [
        "--strip-all -- /usr/bin/prog",
        "--strip-debug -- /usr/lib/libt.a",
        "--strip-debug -- /usr/lib/libt.so",
    ];

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Stage with an executable, shared library, static archive and object, plus
    /// files that are not objects. Only the magic numbers matter to the fake strip.
    fn fake_stage(root: &Path) -> PathBuf {
        let stage = root.join("stage");
        let bin = stage.join("usr/bin");
        let lib = stage.join("usr/lib");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(bin.join("prog"), b"\x7fELF\x02\x01\x01\0").unwrap();
        write_executable(&bin.join("script"), "#!/bin/sh\n");
        std::fs::write(lib.join("libt.so"), b"\x7fELF\x02\x01\x01\0").unwrap();
        std::fs::write(lib.join("libt.a"), b"!<arch>\n").unwrap();
        std::fs::write(lib.join("libc.so"), b"INPUT(-lt)\n").unwrap();
        std::fs::write(lib.join("thin.a"), b"!<thin>\n").unwrap();
        std::fs::create_dir_all(lib.join("pkgconfig")).unwrap();
        std::fs::write(lib.join("pkgconfig/t.pc"), b"Name: t\n").unwrap();
        stage
    }

    /// A strip that, like llvm-strip, rejects `-v`, logs its options and file, and
    /// fails on any file whose name contains `broken`.
    fn fake_strip(root: &Path) -> (PathBuf, PathBuf) {
        let log = root.join("strip.log");
        let tool = root.join("fake-strip");
        write_executable(
            &tool,
            &format!(
                "#!/bin/sh\n\
                 for arg; do [ \"$arg\" = -v ] && {{ echo \"unknown argument '-v'\" >&2; exit 1; }}; done\n\
                 echo \"$*\" >> '{}'\n\
                 case \"$3\" in *broken*) exit 1 ;; esac\n",
                log.display()
            ),
        );
        (strip_bin_dir(root, &tool), log)
    }

    #[test]
    fn postscript_strips_only_objects_with_portable_options() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());
        let (bin, log) = fake_strip(root.path());

        let output = run_postscript(&stage, &bin);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(strip_calls(&log, &stage), EXPECTED_STRIP_CALLS);
    }

    /// Runs the build prescript, a recipe script that unsets `STRIP` as
    /// `cookbook_meson` does, and the postscript, as a `host:` build.
    fn run_host_build(stage: &Path, strip_bin: &Path, strip: Option<&str>) -> Output {
        let host = "x86_64-unknown-linux-gnu";
        // The prescript puts `${COOKBOOK_ROOT}/bin` first in `PATH`.
        let root = strip_bin.parent().unwrap().to_str().unwrap();
        let mut env = vec![
            ("COOKBOOK_ROOT", root),
            ("TARGET", host),
            ("COOKBOOK_HOST_TARGET", host),
            ("GNU_TARGET", "x86_64-linux-gnu"),
        ];
        env.extend(strip.map(|strip| ("STRIP", strip)));
        let script = format!("{BUILD_PRESCRIPT}\nunset STRIP\n{BUILD_POSTSCRIPT}");
        run_strip_script(&script, stage, strip_bin, &env)
    }

    #[test]
    fn host_builds_use_the_strip_redoxer_selected() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());
        let (bin, log) = fake_strip(root.path());
        std::os::unix::fs::symlink(root.path().join("fake-strip"), bin.join("host-strip")).unwrap();

        let output = run_host_build(&stage, &bin, Some("host-strip"));
        assert!(output.status.success(), "{output:?}");
        assert_eq!(strip_calls(&log, &stage), EXPECTED_STRIP_CALLS);
    }

    #[test]
    fn host_builds_without_strip_use_unprefixed_strip() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());
        let (bin, log) = fake_strip(root.path());
        std::os::unix::fs::symlink(root.path().join("fake-strip"), bin.join("strip")).unwrap();

        let output = run_host_build(&stage, &bin, None);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(strip_calls(&log, &stage), EXPECTED_STRIP_CALLS);
    }

    #[test]
    fn postscript_fails_when_the_stage_cannot_be_listed() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());
        let (bin, log) = fake_strip(root.path());
        // A find that lists every file but then reports an error, as it does
        // after an unreadable directory.
        let real_find = find_tool(&["find"]).unwrap();
        write_executable(
            &bin.join("find"),
            &format!("#!/bin/sh\n'{}' \"$@\"\nexit 1\n", real_find.display()),
        );

        let output = run_postscript(&stage, &bin);
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("could not list all the files to strip"),
            "{stderr}"
        );
        // The files that were listed are still stripped.
        assert_eq!(strip_calls(&log, &stage), EXPECTED_STRIP_CALLS[..1]);
    }

    #[test]
    fn postscript_fails_when_strip_fails_on_an_object() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());
        std::fs::write(stage.join("usr/lib/libbroken.so"), b"\x7fELF").unwrap();
        let (bin, log) = fake_strip(root.path());

        let output = run_postscript(&stage, &bin);
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("libbroken.so"), "{stderr}");
        // The rest of the directory is still processed before the build fails.
        assert!(std::fs::read_to_string(log).unwrap().contains("libt.so"));
    }

    #[test]
    fn postscript_leaves_mach_o_host_packages_unstripped() {
        let root = tempfile::tempdir().unwrap();
        let stage = fake_stage(root.path());

        let output = Command::new("bash")
            .arg("-e")
            .arg("-c")
            .arg(BUILD_POSTSCRIPT)
            .env("TARGET", "aarch64-apple-darwin")
            .env("GNU_TARGET", "aarch64-apple-darwin")
            .env("COOKBOOK_STAGE", &stage)
            .env("COOKBOOK_NAME", "strip-test")
            .env_remove("COOKBOOK_NOSTRIP")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    /// Returns the first of `names` that is an executable file in `PATH` or the Rust
    /// toolchain's `llvm-tools` directory.
    fn find_tool(names: &[&str]) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        if let Ok(output) = Command::new("rustc").arg("--print").arg("sysroot").output() {
            let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if let Ok(entries) = std::fs::read_dir(Path::new(&sysroot).join("lib/rustlib")) {
                dirs.extend(entries.flatten().map(|entry| entry.path().join("bin")));
            }
        }
        names
            .iter()
            .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
            .find(|path| path.is_file())
    }

    /// Finds llvm-strip, including Debian's versioned `llvm-strip-<N>` names.
    fn find_llvm_strip() -> Option<PathBuf> {
        let versioned: Vec<String> = (14..=30).rev().map(|v| format!("llvm-strip-{v}")).collect();
        let mut names = vec!["llvm-strip"];
        names.extend(versioned.iter().map(String::as_str));
        find_tool(&names)
    }

    /// Finds a GNU binutils strip.
    fn find_gnu_strip() -> Option<PathBuf> {
        let strip = find_tool(&["x86_64-linux-gnu-strip", "strip"])?;
        let version = Command::new(&strip).arg("--version").output().ok()?;
        String::from_utf8_lossy(&version.stdout)
            .contains("GNU strip")
            .then_some(strip)
    }

    fn run_checked(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(output.status.success(), "{command:?}: {output:?}");
    }

    fn section_names(path: &Path) -> String {
        let output = Command::new("readelf")
            .arg("-SW")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Builds a real executable, shared library, object and static archive with
    /// debug info, strips them through the postscript with `strip`, and checks the
    /// results.
    fn strip_real_objects(strip: &Path) {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("t.c");
        std::fs::write(
            &src,
            "int lib_fn(int x) { return x * 2; }\nint main(void) { return lib_fn(1) - 2; }\n",
        )
        .unwrap();
        let stage = root.path().join("stage");
        let bin = stage.join("usr/bin");
        let lib = stage.join("usr/lib");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        run_checked(
            Command::new("cc")
                .args(["-g", "-o"])
                .arg(bin.join("prog"))
                .arg(&src),
        );
        run_checked(
            Command::new("cc")
                .args(["-g", "-fPIC", "-shared", "-o"])
                .arg(lib.join("libt.so"))
                .arg(&src),
        );
        run_checked(
            Command::new("cc")
                .args(["-g", "-c", "-o"])
                .arg(lib.join("t.o"))
                .arg(&src),
        );
        run_checked(
            Command::new("ar")
                .arg("rcs")
                .arg(lib.join("libt.a"))
                .arg(lib.join("t.o")),
        );
        write_executable(&bin.join("script"), "#!/bin/sh\necho hi\n");
        std::fs::write(lib.join("libc.so"), "INPUT(-lt)\n").unwrap();
        assert!(section_names(&lib.join("libt.so")).contains(".debug_info"));

        let output = run_postscript(&stage, &strip_bin_dir(root.path(), strip));
        assert!(output.status.success(), "{output:?}");

        let prog = section_names(&bin.join("prog"));
        assert!(
            !prog.contains(".symtab") && !prog.contains(".debug"),
            "{prog}"
        );
        for object in ["libt.so", "t.o"] {
            let sections = section_names(&lib.join(object));
            assert!(!sections.contains(".debug"), "{object}: {sections}");
            assert!(sections.contains(".symtab"), "{object}: {sections}");
        }
        let members = root.path().join("members");
        std::fs::create_dir_all(&members).unwrap();
        run_checked(
            Command::new("ar")
                .arg("x")
                .arg(lib.join("libt.a"))
                .current_dir(&members),
        );
        let member = section_names(&members.join("t.o"));
        assert!(
            !member.contains(".debug") && member.contains(".symtab"),
            "{member}"
        );
        assert_eq!(
            std::fs::read_to_string(bin.join("script")).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            std::fs::read_to_string(lib.join("libc.so")).unwrap(),
            "INPUT(-lt)\n"
        );

        // A truncated ELF file is a real strip failure and fails the build.
        std::fs::write(
            lib.join("libbroken.so"),
            &std::fs::read(lib.join("libt.so")).unwrap()[..64],
        )
        .unwrap();
        let output = run_postscript(&stage, &root.path().join("bin"));
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("libbroken.so"),
            "{output:?}"
        );
    }

    fn host_has_elf_tools() -> bool {
        cfg!(target_os = "linux")
            && ["cc", "ar", "readelf"]
                .iter()
                .all(|tool| find_tool(&[tool]).is_some())
    }

    #[test]
    fn postscript_strips_real_objects_with_llvm_strip() {
        match find_llvm_strip() {
            Some(strip) if host_has_elf_tools() => strip_real_objects(&strip),
            _ => eprintln!("skipped: llvm-strip, cc, ar or readelf not found on this Linux host"),
        }
    }

    #[test]
    fn postscript_strips_real_objects_with_gnu_strip() {
        match find_gnu_strip() {
            Some(strip) if host_has_elf_tools() => strip_real_objects(&strip),
            _ => eprintln!("skipped: GNU strip, cc, ar or readelf not found on this Linux host"),
        }
    }
}
