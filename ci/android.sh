#!/usr/bin/env bash
# Shared runner for every Android task, locally and in CI.
#
#   ci/android.sh provision             install the pinned Rust toolchain and targets, cargo-ndk and JDK into $WEZTERM_ANDROID_TOOLCHAIN
#   ci/android.sh native [abi] [variant] cross-build debug (default) or release JNI and separate symbols
#   ci/android.sh build                 assemble the debug APK of every ABI (rebuilding the native libraries first), lint it and run the JVM unit tests
#   ci/android.sh release              assemble, lint, inspect, and record a signed release; private signing files required
#   ci/android.sh check [abi] [variant] cargo check + clippy (-D warnings) + rustdoc for the Android crates
#   ci/android.sh inspect [variant]     gate actual ELF, DEX, manifest, notices, APK, symbols, and signatures
#   ci/android.sh inspect-selftest [variant] prove rejection of missing, stale, and malformed artifacts
#   ci/android.sh install <serial>      install the APK matching the device ABI
#   ci/android.sh test <serial> <suite> check, rebuild, and run serial-only instrumentation (native-load, surface, lifecycle, sshmux, input, reconnect; signed release-load/release-sshmux; all sshmux/input/reconnect suites need the owned fixture)
#
# Machine-specific SDK/NDK locations come from the environment or from the
# untracked ci/android.local.env written by `provision`.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

gradle_prop() { sed -n "s/^$1=//p" android/gradle.properties; }
declare -A RUST_TARGET=([arm64-v8a]=aarch64-linux-android [x86_64]=x86_64-linux-android)
declare -A ELF_MACHINE=([arm64-v8a]=AArch64 [x86_64]="Advanced Micro Devices X86-64")
IFS=, read -ra ABIS <<< "$(gradle_prop wezterm.abis)"
PERMITTED_NEEDED=(libandroid.so libc.so libdl.so liblog.so libm.so)
REQUIRED_JNI_EXPORTS=(
  Java_org_wezterm_android_NativeApp_nativeInitialize
  Java_org_wezterm_android_NativeApp_nativeDiagnosticFault
  Java_org_wezterm_android_NativeApp_nativeTerminalStart
  Java_org_wezterm_android_NativeApp_nativeSurfaceCreated
  Java_org_wezterm_android_NativeApp_nativeSurfaceChanged
  Java_org_wezterm_android_NativeApp_nativeSurfaceDestroyed
  Java_org_wezterm_android_NativeApp_nativeSurfaceStatus
  Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceFrames
  Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceState
  Java_org_wezterm_android_NativeApp_nativeAwaitRenderFailures
  Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceChange
  Java_org_wezterm_android_NativeApp_nativeSelectWindow
  Java_org_wezterm_android_NativeApp_nativeNextRequest
  Java_org_wezterm_android_NativeApp_nativeClipboardText
  Java_org_wezterm_android_NativeApp_nativeDiagnosticGui
  Java_org_wezterm_android_NativeApp_nativeConnect
  Java_org_wezterm_android_NativeApp_nativeConnectionStatus
  Java_org_wezterm_android_NativeApp_nativeAwaitConnectionChange
  Java_org_wezterm_android_NativeApp_nativeAnswerHostTrust
  Java_org_wezterm_android_NativeApp_nativeAnswerText
  Java_org_wezterm_android_NativeApp_nativeCancelConnect
  Java_org_wezterm_android_NativeApp_nativeDisconnect
  Java_org_wezterm_android_NativeApp_nativeDiagnosticConnection
  Java_org_wezterm_android_NativeApp_nativeImportIdentity
  Java_org_wezterm_android_NativeApp_nativeDiagnosticMux
  Java_org_wezterm_android_NativeApp_nativeInputPreedit
  Java_org_wezterm_android_NativeApp_nativeInputCommit
  Java_org_wezterm_android_NativeApp_nativeIsPasteKey
  Java_org_wezterm_android_NativeApp_nativeInputKey
  Java_org_wezterm_android_NativeApp_nativeInputPaste
  Java_org_wezterm_android_NativeApp_nativeInputTouch
  Java_org_wezterm_android_NativeApp_nativeDiagnosticActivePane
  Java_org_wezterm_android_NativeApp_nativeDiagnosticHeldObservations
)
# sshmux, input, reconnect and release-sshmux use the existing owned fixture.
SSHMUX_FIXTURE=${WEZTERM_SSHMUX_FIXTURE_DIR:-target/android-sshmux-fixture}
SSHMUX_DEVICE_DIR=/data/local/tmp/wezterm-sshmux
SSHMUX_PICKER_DIR=/sdcard/Download/wezterm-fixture

CARGO_NDK_VERSION=4.1.2
JDK_VERSION="17.0.20.1+1"
JDK_ARCHIVE="OpenJDK17U-jdk_x64_linux_hotspot_17.0.20.1_1.tar.gz"
JDK_SHA256=3808d1d15e3ec6bd5b84057fb5d84c33d8a1536a258146bcea2e603fc726e08e
JDK_URL="https://github.com/adoptium/temurin17-binaries/releases/download/jdk-17.0.20.1%2B1/$JDK_ARCHIVE"
ANDROID_MIN_API=$(gradle_prop wezterm.minSdk)
NDK_VERSION=$(gradle_prop wezterm.ndkVersion)
BUILD_TOOLS_VERSION=$(gradle_prop wezterm.buildToolsVersion)
RUST_TOOLCHAIN=$(gradle_prop wezterm.rustToolchain)
export RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN

TOOLCHAIN_DIR=${WEZTERM_ANDROID_TOOLCHAIN:-$HOME/.local/share/wezterm-android-toolchain}
LOCAL_ENV=ci/android.local.env
if [ -f "$LOCAL_ENV" ]; then
  # shellcheck disable=SC1090
  source "$LOCAL_ENV"
fi
: "${ANDROID_HOME:?set ANDROID_HOME (or run provision with it set) to the Android SDK root}"
export ANDROID_HOME
export ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$NDK_VERSION}
export JAVA_HOME=$TOOLCHAIN_DIR/jdk-$JDK_VERSION
export GRADLE_USER_HOME=$TOOLCHAIN_DIR/gradle-home
export PATH=$TOOLCHAIN_DIR/cargo-ndk/bin:$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$PATH
# Host pkg-config must never satisfy a target build. The pkg-config crate
# refuses cross builds unless this is set, which is the behavior we want.
unset PKG_CONFIG_ALLOW_CROSS

NDK_BIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
SYSROOT_LIB=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib
BUILD_TOOLS=$ANDROID_HOME/build-tools/$BUILD_TOOLS_VERSION
JNI_LIBS=android/app/src/main/jniLibs
SYMBOLS_DIR=target/android-symbols
INSPECT_DIR=target/android-inspect
APK_DIR=android/app/build/outputs/apk/debug
LIB=libwezterm_android.so

die() { echo "android.sh: $*" >&2; exit 1; }
require_abi() { [ -n "${RUST_TARGET[$1]:-}" ] || die "unknown ABI '$1' (known: ${ABIS[*]})"; }
# Per-ABI commands take one ABI or run for every ABI.
abis_or_all() { if [ $# -gt 0 ]; then require_abi "$1"; echo "$1"; else echo "${ABIS[@]}"; fi; }

cmd_provision() {
  mkdir -p "$TOOLCHAIN_DIR/downloads" "$GRADLE_USER_HOME"
  rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --component clippy --no-self-update
  for abi in "${ABIS[@]}"; do rustup target add --toolchain "$RUST_TOOLCHAIN" "${RUST_TARGET[$abi]}"; done
  if ! "$TOOLCHAIN_DIR/cargo-ndk/bin/cargo-ndk" ndk --version 2>/dev/null | grep -q "cargo-ndk $CARGO_NDK_VERSION"; then
    cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked --root "$TOOLCHAIN_DIR/cargo-ndk"
  fi
  if [ ! -x "$JAVA_HOME/bin/java" ]; then
    archive=$TOOLCHAIN_DIR/downloads/$JDK_ARCHIVE
    [ -f "$archive" ] || curl -sSL --fail -o "$archive" "$JDK_URL"
    echo "$JDK_SHA256  $archive" | sha256sum -c -
    mkdir -p "$JAVA_HOME"
    tar -xzf "$archive" -C "$JAVA_HOME" --strip-components=1
  fi
  "$JAVA_HOME/bin/java" -version
  [ -d "$ANDROID_NDK_HOME" ] || die "NDK $NDK_VERSION not found at $ANDROID_NDK_HOME"
  [ -d "$BUILD_TOOLS" ] || die "build-tools $BUILD_TOOLS_VERSION not found at $BUILD_TOOLS"
  [ -d "$ANDROID_HOME/platforms/android-$(gradle_prop wezterm.compileSdk)" ] || die "platform android-$(gradle_prop wezterm.compileSdk) missing"
  printf 'ANDROID_HOME=%q\nANDROID_NDK_HOME=%q\n' "$ANDROID_HOME" "$ANDROID_NDK_HOME" > "$LOCAL_ENV"
  printf 'sdk.dir=%s\n' "$ANDROID_HOME" > android/local.properties
  echo "wrote $LOCAL_ENV and android/local.properties"
  rustc --version
  cargo ndk --version
}

cargo_ndk() {
  local abi=$1; shift
  cargo ndk -t "$abi" --platform "$ANDROID_MIN_API" "$@"
}

set_variant() {
  VARIANT=${1:-debug}
  case "$VARIANT" in debug|release) ;; *) die "unknown build variant '$VARIANT'" ;; esac
  if [ "$VARIANT" = release ]; then
    JNI_LIBS=target/android-release/jniLibs
    SYMBOLS_DIR=target/android-release/symbols
    INSPECT_DIR=target/android-release/inspect
  else
    JNI_LIBS=android/app/src/main/jniLibs
    SYMBOLS_DIR=target/android-symbols
    INSPECT_DIR=target/android-inspect
  fi
  APK_DIR=android/app/build/outputs/apk/$VARIANT
}
VARIANT=debug

cmd_native() {
  local abi
  set_variant "${2:-debug}"
  local flags=()
  if [ "$VARIANT" = release ]; then flags=(--release); fi
  local selection=()
  if [ -n "${1:-}" ]; then selection=("$1"); fi
  for abi in $(abis_or_all "${selection[@]}"); do
    if [ "$VARIANT" = release ]; then
      CARGO_PROFILE_RELEASE_DEBUG=2 cargo_ndk "$abi" build --locked -p wezterm-android "${flags[@]}"
    else
      cargo_ndk "$abi" build --locked -p wezterm-android
    fi
    local built=target/${RUST_TARGET[$abi]}/$VARIANT/$LIB
    mkdir -p "$SYMBOLS_DIR/$abi" "$JNI_LIBS/$abi"
    cp "$built" "$SYMBOLS_DIR/$abi/$LIB"
    # Gradle copies the jniLibs input several times; DWARF stays only in
    # the symbols copy.  --strip-debug leaves .text and the symbol tables.
    "$NDK_BIN/llvm-strip" --strip-debug -o "$JNI_LIBS/$abi/$LIB" "$built"
  done
}

gradle() { ./android/gradlew -p android --console=plain "$@"; }

cmd_build() {
  # lintDebug fails on any error, including a framework call above minSdk (NewApi).
  gradle :app:assembleDebug :app:lintDebug :app:testDebugUnitTest
  ls -l "$APK_DIR"/*.apk
}

signing_certificate() {
  : "${WEZTERM_ANDROID_KEYSTORE:?release keystore required}"
  : "${WEZTERM_ANDROID_KEY_ALIAS:?release alias required}"
  : "${WEZTERM_ANDROID_STORE_PASSWORD_FILE:?release store password file required}"
  : "${WEZTERM_ANDROID_KEY_PASSWORD_FILE:?release key password file required}"
  "$JAVA_HOME/bin/keytool" -exportcert -keystore "$WEZTERM_ANDROID_KEYSTORE" \
    -alias "$WEZTERM_ANDROID_KEY_ALIAS" -storepass:file "$WEZTERM_ANDROID_STORE_PASSWORD_FILE" | sha256sum | cut -d' ' -f1
}

cmd_release() {
  set_variant release
  signing_certificate > /dev/null
  gradle :app:assembleRelease :app:lintRelease :app:testReleaseUnitTest
  # Preserve inputs and artifacts even when the inspection gate rejects them.
  local receipts=target/android-release/receipts
  mkdir -p "$receipts"
  git rev-parse HEAD > "$receipts/parent.txt"
  git diff --binary > "$receipts/source.diff"
  git submodule status --recursive > "$receipts/submodules.txt"
  while IFS= read -r -d '' file; do
    if [ -f "$file" ]; then sha256sum "$file"; fi
  done < <(git ls-files --cached --others --exclude-standard -z) > "$receipts/source.sha256"
  {
    rustc -vV
    cargo ndk --version
    "$JAVA_HOME/bin/java" -version 2>&1
    gradle --version
    printf 'ANDROID_HOME=%s\nANDROID_NDK_HOME=%s\nNDK_BIN=%s\n' "$ANDROID_HOME" "$ANDROID_NDK_HOME" "$NDK_BIN"
    cat "$ANDROID_NDK_HOME/source.properties"
    "$NDK_BIN/clang" --version
    "$NDK_BIN/llvm-readelf" --version
    "$NDK_BIN/llvm-objcopy" --version
    sha256sum "$NDK_BIN/clang" "$NDK_BIN/llvm-readelf" "$NDK_BIN/llvm-objcopy"
    cat android/gradle.properties android/gradle/wrapper/gradle-wrapper.properties
    printf 'native profile: release; opt-level=3; debug=2; locked Cargo resolution\n'
  } > "$receipts/tools.txt"
  find "$GRADLE_USER_HOME/caches/modules-2/files-2.1" -type f -print0 | sort -z | xargs -0 sha256sum > "$receipts/gradle-inputs.sha256"
  find "$APK_DIR" "$JNI_LIBS" "$SYMBOLS_DIR" target/android-notices/assets -type f -print0 | sort -z | xargs -0 sha256sum > "$receipts/artifacts.sha256"
  signing_certificate > "$receipts/certificate.sha256"
  cmd_inspect release
}

cmd_check() {
  local abi selection=() flags=()
  set_variant "${2:-debug}"
  if [ -n "${1:-}" ]; then selection=("$1"); fi
  if [ "$VARIANT" = release ]; then
    flags=(--release)
    export CARGO_PROFILE_RELEASE_DEBUG=2
  fi
  for abi in $(abis_or_all "${selection[@]}"); do
    cargo_ndk "$abi" check --locked -p wezterm-android -p window "${flags[@]}"
    cargo_ndk "$abi" clippy --locked -p wezterm-android --all-targets --no-deps "${flags[@]}" -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo_ndk "$abi" doc --locked -p wezterm-android --no-deps "${flags[@]}"
  done
}

# Symbols exported at ANDROID_MIN_API by the libraries a .so links (its
# DT_NEEDED set); an unresolved strong symbol outside this set is a load failure.
sysroot_exports() {
  local triple=$1 so=$2
  for lib in $(dt_needed "$so"); do
    "$NDK_BIN/llvm-nm" -D --defined-only "$SYSROOT_LIB/$triple/$ANDROID_MIN_API/$lib" 2>/dev/null || true
  done | awk 'NF==3 {print $3}' | sed 's/@.*//' | sort -u
}

dt_needed() { "$NDK_BIN/llvm-readelf" -d "$1" 2>/dev/null | awk '/NEEDED/ {gsub(/[\[\]]/, "", $NF); print $NF}'; }
readelf_field() { "$NDK_BIN/llvm-readelf" -h "$1" 2>/dev/null | awk -v key="$2:" '$1==key {sub(/^[^:]*:[ ]*/, ""); print}'; }
defined_symbols_sha256() { "$NDK_BIN/llvm-nm" --defined-only "$1" 2>/dev/null | sort | sha256sum | cut -d' ' -f1; }
has_section() { "$NDK_BIN/llvm-readelf" -S "$1" 2>/dev/null | grep -c " $2 " || true; }
text_sha256() { "$NDK_BIN/llvm-objcopy" --dump-section .text=/dev/stdout "$1" /dev/null 2>/dev/null | sha256sum | cut -d' ' -f1; }

dependency_inventory() {
  local abi=$1 out=$2
  cargo tree --locked --target "${RUST_TARGET[$abi]}" -p wezterm-android --edges normal,build \
    --prefix none --format "{p} {l}" | sed 's| (/.*)||' | sort -u > "$out"
  echo "### dependency inventory: $out ($(wc -l < "$out") crates)"
  awk '{ $1=""; $2=""; print }' "$out" | sort | uniq -c | sort -rn | head -12 | sed 's/^/  /'
}

# Gate state for one inspect_tree run.
OUT=
FAILURES=0
ok() { echo "ok   $*" >> "$OUT"; }
fail() { echo "FAIL $*" >> "$OUT"; FAILURES=$((FAILURES + 1)); }
assert_eq() { local what=$1 actual=$2 expected=$3; if [ "$actual" = "$expected" ]; then ok "$what: $actual"; else fail "$what: got '$actual', expected '$expected'"; fi; }
assert_file() { if [ -f "$2" ]; then ok "$1 present: $2"; else fail "$1 missing: $2"; fi; }

inspect_so() {
  local abi=$1 so=$2 symbols=$3 out_dir=$4
  local triple=${RUST_TARGET[$abi]} ndk_major=${NDK_VERSION%%.*} ndk_build=${NDK_VERSION##*.}
  echo "### native library $so ($(stat -c %s "$so") bytes, sha256 $(sha256sum "$so" | cut -d' ' -f1))" >> "$OUT"
  assert_eq "$abi ELF class" "$(readelf_field "$so" Class)" "ELF64"
  assert_eq "$abi ELF type" "$(readelf_field "$so" Type)" "DYN (Shared object file)"
  assert_eq "$abi ELF machine" "$(readelf_field "$so" Machine)" "${ELF_MACHINE[$abi]}"
  local load_aligns; load_aligns=$("$NDK_BIN/llvm-readelf" -l "$so" 2>/dev/null | awk '$1=="LOAD" {print $NF}' | sort -u | tr '\n' ' ')
  assert_eq "$abi LOAD segment alignment (16 KiB pages)" "${load_aligns% }" "0x4000"
  assert_eq "$abi DT_RPATH/DT_RUNPATH" "$("$NDK_BIN/llvm-readelf" -d "$so" 2>/dev/null | grep -cE 'RPATH|RUNPATH')" "0"
  local needed; needed=$(dt_needed "$so" | sort | tr '\n' ' ')
  local lib disallowed=""
  for lib in $needed; do
    case " ${PERMITTED_NEEDED[*]} " in *" $lib "*) ;; *) disallowed="$disallowed $lib" ;; esac
  done
  assert_eq "$abi DT_NEEDED outside {${PERMITTED_NEEDED[*]}} [$needed]" "${disallowed# }" ""
  local comment; comment=$("$NDK_BIN/llvm-readelf" -p .comment "$so" 2>/dev/null || true)
  assert_eq "$abi built by pinned rustc" "$(grep -o "rustc version $RUST_TOOLCHAIN " <<< "$comment" | head -1)" "rustc version $RUST_TOOLCHAIN "
  "$NDK_BIN/llvm-readelf" -p .note.android.ident "$so" > "$out_dir/$abi.android-ident.txt" 2> "$out_dir/$abi.android-ident.stderr" || true
  "$NDK_BIN/llvm-readelf" -n -x .note.android.ident "$so" > "$out_dir/$abi.android-notes.txt" 2>&1 || true
  "$NDK_BIN/llvm-objcopy" --dump-section .note.android.ident="$out_dir/$abi.android-ident.bin" "$so" /dev/null > "$out_dir/$abi.android-ident-objcopy.txt" 2>&1 || true
  local ident; ident=$(< "$out_dir/$abi.android-ident.txt")
  assert_eq "$abi .note.android.ident names NDK r$ndk_major build $ndk_build" "$(grep -cE "^\[ *[0-9a-f]+\] (r$ndk_major|$ndk_build)$" <<< "$ident")" "2"
  "$NDK_BIN/llvm-nm" -D --defined-only "$so" 2>/dev/null | awk '$2=="T" && $3 ~ /^Java_/ {print $3}' | sort > "$out_dir/$abi.jni-exports"
  local export
  for export in "${REQUIRED_JNI_EXPORTS[@]}"; do
    if [ "$VARIANT" = release ]; then
      case "$export" in *nativeDiagnostic*|*nativeAwait*)
        assert_eq "$abi excludes $export" "$(grep -cx "$export" "$out_dir/$abi.jni-exports" || true)" "0"
        continue ;;
      esac
    fi
    if grep -qx "$export" "$out_dir/$abi.jni-exports"; then ok "$abi exports $export"; else fail "$abi does not export $export"; fi
  done
  if [ "$VARIANT" = release ]; then
    assert_eq "$abi release diagnostic or wait-only JNI exports" "$(grep -cE 'nativeDiagnostic|nativeAwait' "$out_dir/$abi.jni-exports" || true)" "0"
  fi
  "$NDK_BIN/llvm-nm" -D --undefined-only "$so" 2>/dev/null | awk '{print $(NF-1) " " $NF}' | sed 's/@.*//' | sort -u -k2 > "$out_dir/$abi.undefined"
  sysroot_exports "$triple" "$so" > "$out_dir/$abi.sysroot"
  awk -v exports="$out_dir/$abi.sysroot" 'BEGIN { while ((getline line < exports) > 0) have[line]=1 } !($2 in have)' "$out_dir/$abi.undefined" > "$out_dir/$abi.unresolved"
  local strong; strong=$(awk '$1=="U"' "$out_dir/$abi.unresolved" | wc -l)
  assert_eq "$abi strong symbols unresolved against the API $ANDROID_MIN_API sysroot (weak tolerated: $(awk '$1=="w" {print $2}' "$out_dir/$abi.unresolved" | tr '\n' ' '))" "$strong" "0"
  assert_file "$abi unstripped symbols" "$symbols"
  if [ -f "$symbols" ]; then
    assert_eq "$abi unstripped symbols carry the shipped .text" "$(text_sha256 "$symbols")" "$(text_sha256 "$so")"
    assert_eq "$abi unstripped symbols define the shipped symbols" "$(defined_symbols_sha256 "$symbols")" "$(defined_symbols_sha256 "$so")"
    assert_eq "$abi DWARF sections (symbols / jniLibs)" "$(has_section "$symbols" .debug_info) / $(has_section "$so" .debug_info)" "1 / 0"
  fi
}

inspect_apk() {
  local abi=$1 apk=$2 so=$3 out_dir=$4
  echo "### APK $apk ($(stat -c %s "$apk") bytes, sha256 $(sha256sum "$apk" | cut -d' ' -f1))" >> "$OUT"
  local badging; badging=$("$BUILD_TOOLS/aapt2" dump badging "$apk" 2>/dev/null || true)
  echo "$badging" | grep -E "^package|sdkVersion|targetSdkVersion|native-code|uses-permission" | sed 's/^/  /' >> "$OUT"
  assert_eq "$abi APK native-code" "$(grep -o "^native-code: .*" <<< "$badging")" "native-code: '$abi'"
  local manifest; manifest=$("$BUILD_TOOLS/aapt2" dump xmltree --file AndroidManifest.xml "$apk" 2>/dev/null || true)
  assert_eq "$abi APK minSdkVersion" "$(grep -o 'minSdkVersion([^)]*)=[0-9]*' <<< "$manifest" | sed 's/.*=//')" "$ANDROID_MIN_API"
  assert_eq "$abi APK targetSdkVersion" "$(grep -o 'targetSdkVersion([^)]*)=[0-9]*' <<< "$manifest" | sed 's/.*=//')" "$(gradle_prop wezterm.targetSdk)"
  assert_eq "$abi APK native libraries" "$(unzip -Z1 "$apk" 2>/dev/null | grep '\.so$' | sort | tr '\n' ' ')" "lib/$abi/$LIB "
  local packaged=$out_dir/$abi.packaged.so
  if unzip -p "$apk" "lib/$abi/$LIB" > "$packaged" 2>/dev/null && [ -s "$packaged" ]; then
    assert_eq "$abi APK packages the current jniLibs .text" "$(text_sha256 "$packaged")" "$(text_sha256 "$so")"
    assert_eq "$abi APK library ELF machine" "$(readelf_field "$packaged" Machine)" "${ELF_MACHINE[$abi]}"
  else
    fail "$abi APK does not contain lib/$abi/$LIB"
  fi
  rm -f "$packaged"
  local verify; verify=$("$BUILD_TOOLS/apksigner" verify --print-certs -v "$apk" 2>&1 || true)
  assert_eq "$abi APK signature verifies" "$(grep -c '^Verifies$' <<< "$verify")" "1"
  assert_eq "$abi APK Signature Scheme v2" "$(grep -o 'Verified using v2 scheme (APK Signature Scheme v2): [a-z]*' <<< "$verify")" "Verified using v2 scheme (APK Signature Scheme v2): true"
  grep -E 'DN:|SHA-256 digest' <<< "$verify" | sed 's/^/  /' >> "$OUT"
  grep -o 'certificate SHA-256 digest: [0-9a-f]*' <<< "$verify" | head -1 >> "$out_dir/cert-digests"
  if [ "$VARIANT" = release ]; then
    local expected_certificate
    expected_certificate=$(signing_certificate)
    assert_eq "$abi release signing identity" "$(grep -o 'certificate SHA-256 digest: [0-9a-f]*' <<< "$verify" | head -1 | cut -d' ' -f4)" "$expected_certificate"
    printf '%s\n' "$manifest" > "$out_dir/$abi.manifest.txt"
    assert_eq "$abi release diagnostic Activity" "$(grep -c DiagnosticActivity <<< "$manifest" || true)" "0"
    assert_eq "$abi release debuggable" "$(grep -cE 'debuggable.*=(true|0xffffffff)' <<< "$manifest" || true)" "0"
    local dex=$out_dir/$abi.dex.txt
    : > "$dex"
    local entry
    while IFS= read -r entry; do
      unzip -p "$apk" "$entry" | strings >> "$dex"
    done < <(unzip -Z1 "$apk" | grep -E '^classes[0-9]*\.dex$')
    assert_eq "$abi release DEX diagnostic entries and config bypass" "$(grep -cE 'DiagnosticActivity|nativeDiagnostic|nativeAwait|DIAGNOSTIC_APPLET|CONFIG_OVERRIDES' "$dex" || true)" "0"
    local notice
    while IFS= read -r notice; do
      assert_eq "$abi bundled notice $notice" "$(unzip -p "$apk" "assets/notices/$notice" | sha256sum | cut -d' ' -f1)" "$(sha256sum "target/android-notices/assets/notices/$notice" | cut -d' ' -f1)"
    done < <(find target/android-notices/assets/notices -type f -printf '%P\n' | sort)
    assert_file "$abi notice index" target/android-notices/assets/notices/index.json
  fi
  if "$BUILD_TOOLS/zipalign" -c -P 16 -v 4 "$apk" > "$out_dir/$abi.zipalign" 2>&1; then ok "$abi APK zipalign 4-byte, .so on 16 KiB pages"; else fail "$abi APK zipalign (see $out_dir/$abi.zipalign)"; fi
}

# inspect_tree <jniLibs dir> <symbols dir> <apk dir> <out dir>: writes
# <out dir>/inspect.txt and returns nonzero if any assertion failed.
inspect_tree() {
  local jni=$1 symbols=$2 apks=$3 out_dir=$4
  mkdir -p "$out_dir"
  OUT=$out_dir/inspect.txt
  FAILURES=0
  {
    printf 'ANDROID_NDK_HOME=%s\nNDK_BIN=%s\nexpected NDK=%s\n' "$ANDROID_NDK_HOME" "$NDK_BIN" "$NDK_VERSION"
    cat "$ANDROID_NDK_HOME/source.properties"
    "$NDK_BIN/llvm-readelf" --version
    "$NDK_BIN/llvm-objcopy" --version
    sha256sum "$NDK_BIN/llvm-readelf" "$NDK_BIN/llvm-objcopy"
  } > "$out_dir/ndk-tools.txt" 2>&1
  : > "$OUT" "$out_dir/cert-digests"
  echo "# wezterm-android inspection $(date -u +%Y-%m-%dT%H:%M:%SZ) head=$(git rev-parse HEAD)" >> "$OUT"
  echo "minSdk=$ANDROID_MIN_API ndk=$NDK_VERSION build-tools=$BUILD_TOOLS_VERSION rust=$RUST_TOOLCHAIN ($(rustc --version))" >> "$OUT"
  local abi
  for abi in "${ABIS[@]}"; do
    echo "## $abi" >> "$OUT"
    local so=$jni/$abi/$LIB apk=$apks/app-$abi-$VARIANT.apk
    assert_file "$abi native library" "$so"
    [ -f "$so" ] && inspect_so "$abi" "$so" "$symbols/$abi/$LIB" "$out_dir"
    assert_file "$abi APK" "$apk"
    [ -f "$apk" ] && [ -f "$so" ] && inspect_apk "$abi" "$apk" "$so" "$out_dir"
  done
  assert_eq "APKs signed by one certificate" "$(sort -u "$out_dir/cert-digests" | wc -l)" "1"
  echo "## result: $FAILURES failed assertion(s)" >> "$OUT"
  [ "$FAILURES" -eq 0 ]
}

cmd_inspect() {
  set_variant "${1:-debug}"
  local status=0
  inspect_tree "$JNI_LIBS" "$SYMBOLS_DIR" "$APK_DIR" "$INSPECT_DIR" || status=$?
  local abi
  for abi in "${ABIS[@]}"; do
    dependency_inventory "$abi" "$INSPECT_DIR/dependency-inventory-$abi.txt" >> "$OUT"
  done
  cat "$OUT"
  return $status
}

# Fixture trees are symlinks to the shipping artifacts, so a case that removes
# or replaces an entry never touches the real file.
fixture() {
  local dir=$INSPECT_DIR/selftest/$1
  rm -rf "$dir"; mkdir -p "$dir"
  cp -rs "$ROOT/$JNI_LIBS" "$dir/jniLibs"
  cp -rs "$ROOT/$SYMBOLS_DIR" "$dir/symbols"
  cp -rs "$ROOT/$APK_DIR" "$dir/apk"
  echo "$dir"
}

expect_reject() {
  local name=$1 needle=$2 dir=$INSPECT_DIR/selftest/$1
  if inspect_tree "$dir/jniLibs" "$dir/symbols" "$dir/apk" "$dir/out"; then die "selftest $name: gate accepted a bad tree"; fi
  grep -q "^FAIL $needle" "$dir/out/inspect.txt" || die "selftest $name: rejected, but not for '$needle' (see $dir/out/inspect.txt)"
  echo "selftest $name: rejected, $(grep -c '^FAIL' "$dir/out/inspect.txt") failed assertion(s) including '$needle'"
}

cmd_inspect_selftest() {
  set_variant "${1:-debug}"
  local dir
  dir=$(fixture accepts-shipping-tree)
  inspect_tree "$dir/jniLibs" "$dir/symbols" "$dir/apk" "$dir/out" || die "selftest: gate rejects the shipping tree (see $dir/out/inspect.txt)"
  echo "selftest accepts-shipping-tree: accepted"

  dir=$(fixture missing-apk); rm "$dir/apk/app-arm64-v8a-$VARIANT.apk"
  expect_reject missing-apk "arm64-v8a APK missing"

  dir=$(fixture missing-symbols); rm "$dir/symbols/x86_64/$LIB"
  expect_reject missing-symbols "x86_64 unstripped symbols missing"

  dir=$(fixture wrong-machine); ln -sf "$ROOT/$JNI_LIBS/arm64-v8a/$LIB" "$dir/jniLibs/x86_64/$LIB"
  expect_reject wrong-machine "x86_64 ELF machine"

  dir=$(fixture host-library)
  local host_lib; host_lib=$(ldconfig -p | awk '$1=="libm.so.6" && /x86-64/ && !found {print $NF; found=1}')
  [ -n "$host_lib" ] || die "selftest: no host x86-64 libm.so.6 to use as a fixture"
  ln -sf "$host_lib" "$dir/jniLibs/x86_64/$LIB"
  expect_reject host-library "x86_64 built by pinned rustc"
  grep -q "^FAIL x86_64 does not export Java_org_wezterm_android_NativeApp_nativeInitialize" "$dir/out/inspect.txt" || die "selftest host-library: JNI export check did not fire"

  dir=$(fixture truncated-library); rm "$dir/jniLibs/arm64-v8a/$LIB"; head -c 4096 "$JNI_LIBS/arm64-v8a/$LIB" > "$dir/jniLibs/arm64-v8a/$LIB"
  expect_reject truncated-library "arm64-v8a LOAD segment alignment"

  dir=$(fixture stale-package); rm "$dir/jniLibs/x86_64/$LIB" "$dir/symbols/x86_64/$LIB"
  unzip -p "$APK_DIR/app-x86_64-$VARIANT.apk" "lib/x86_64/$LIB" > "$dir/jniLibs/x86_64/$LIB"
  local text_off; text_off=$("$NDK_BIN/llvm-readelf" -S "$dir/jniLibs/x86_64/$LIB" | awk '{ for (i = 1; i <= NF; i++) if ($i == ".text") print $(i + 3) }')
  printf 'stale' | dd of="$dir/jniLibs/x86_64/$LIB" bs=1 seek="$(( 16#$text_off + 64 ))" conv=notrunc status=none
  cp "$dir/jniLibs/x86_64/$LIB" "$dir/symbols/x86_64/$LIB"
  expect_reject stale-package "x86_64 APK packages the current jniLibs .text"
  rm "$dir/jniLibs/x86_64/$LIB" "$dir/symbols/x86_64/$LIB"

  dir=$(fixture symbols-without-dwarf); ln -sf "$ROOT/$JNI_LIBS/x86_64/$LIB" "$dir/symbols/x86_64/$LIB"
  expect_reject symbols-without-dwarf "x86_64 DWARF sections (symbols / jniLibs): got '0 / 0'"

  dir=$(fixture dwarf-in-jnilibs); ln -sf "$ROOT/$SYMBOLS_DIR/x86_64/$LIB" "$dir/jniLibs/x86_64/$LIB"
  expect_reject dwarf-in-jnilibs "x86_64 DWARF sections (symbols / jniLibs): got '1 / 1'"

  dir=$(fixture foreign-symbols); ln -sf "$ROOT/$SYMBOLS_DIR/arm64-v8a/$LIB" "$dir/symbols/x86_64/$LIB"
  expect_reject foreign-symbols "x86_64 unstripped symbols carry the shipped .text"

  dir=$(fixture altered-symbol-table); rm "$dir/jniLibs/x86_64/$LIB"
  "$NDK_BIN/llvm-objcopy" --add-symbol selftest_extra=.text:0,global "$JNI_LIBS/x86_64/$LIB" "$dir/jniLibs/x86_64/$LIB"
  expect_reject altered-symbol-table "x86_64 unstripped symbols define the shipped symbols"
  rm "$dir/jniLibs/x86_64/$LIB"

  echo "selftest: all cases behaved; receipts under $INSPECT_DIR/selftest/*/out/inspect.txt"
}

device_abi() { adb -s "$1" shell getprop ro.product.cpu.abi | tr -d '\r'; }

cmd_install() {
  local serial=${1:?serial}
  set_variant "${2:-debug}"
  local abi; abi=$(device_abi "$serial"); require_abi "$abi"
  local apk=$APK_DIR/app-$abi-$VARIANT.apk
  [ -f "$apk" ] || die "no APK for $abi at $apk; run build first"
  adb -s "$serial" install -r "$apk"
}

shell_quote() { printf "'%s'" "${1//\'/\'\\\'\'}"; }

cmd_test() (
  local serial=${1:?explicit serial required} suite=${2:?suite}
  case "$serial" in -*|*[[:space:]]*) die "invalid explicit serial '$serial'" ;; esac
  local classes class
  classes=$(uv run --no-project python ci/android_instrument.py plan "$suite")
  local results=android/app/build/outputs/androidTest-results
  mkdir -p "$results"
  if [ -e "$results/$suite" ]; then
    mv "$results/$suite" "$(mktemp -d "$results/$suite.previous.XXXXXX")/results"
  fi
  results=$results/$suite
  mkdir -m 700 "$results"
  local logcat_pid= logcat_exit= fixture_files=0
  stop_logcat() {
    if [ -n "$logcat_pid" ]; then
      kill "$logcat_pid" 2>/dev/null || true
      local status=0
      wait "$logcat_pid" || status=$?
      printf '%s\n' "$status" > "$logcat_exit"
      logcat_pid=
    fi
  }
  cleanup() {
    local status=$?
    stop_logcat
    if [ "$fixture_files" = 1 ]; then
      if [ "$suite" = release-sshmux ]; then
        adb -s "$serial" pull "$SSHMUX_DEVICE_DIR/release-shots" "$results/" > "$results/screenshots-pull.txt" 2>&1 || true
        adb -s "$serial" shell rm -rf "$SSHMUX_DEVICE_DIR/release-shots" || status=1
      fi
      adb -s "$serial" shell rm -rf "$SSHMUX_PICKER_DIR" "$SSHMUX_DEVICE_DIR/fixture.properties" || status=1
    fi
    exit "$status"
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' HUP TERM
  local fixture_suite=0
  case "$suite" in sshmux|input|reconnect|release-sshmux) fixture_suite=1 ;; esac
  if [ "$fixture_suite" = 1 ]; then
    [ -f "$SSHMUX_FIXTURE/device/fixture.properties" ] || die "the $suite suite needs the owned fixture: ci/android-sshmux-fixture.sh up <address>"
  fi
  if [ "$suite" = sshmux ]; then
    if grep -qx 'empty=blocked' "$SSHMUX_FIXTURE/device/fixture.properties"; then
      echo "BLOCKED: required empty mux fixture unavailable; no method omitted" > "$results/BLOCKED-empty.txt"
      die "required empty mux fixture unavailable"
    fi
  fi
  local abi; abi=$(device_abi "$serial"); require_abi "$abi"
  local variant=debug
  case "$suite" in release-load|release-sshmux) variant=release ;; esac
  cmd_check "$abi" "$variant"
  if [ "$variant" = release ]; then
    signing_certificate > /dev/null
    gradle -Pwezterm.releaseTests=true :app:assembleRelease :app:assembleReleaseAndroidTest
    cmd_inspect release
  else
    gradle :app:assembleDebug :app:assembleDebugAndroidTest
  fi
  cmd_install "$serial" "$variant"
  adb -s "$serial" install -r "android/app/build/outputs/apk/androidTest/$variant/app-$variant-androidTest.apk"
  if [ "$fixture_suite" = 1 ]; then
    fixture_files=1
    adb -s "$serial" shell "rm -rf $SSHMUX_DEVICE_DIR $SSHMUX_PICKER_DIR && mkdir -p $SSHMUX_DEVICE_DIR $SSHMUX_PICKER_DIR"
    adb -s "$serial" push "$SSHMUX_FIXTURE/device/fixture.properties" "$SSHMUX_DEVICE_DIR/" > /dev/null
    adb -s "$serial" push "$SSHMUX_FIXTURE"/device/wezterm-fixture-* "$SSHMUX_PICKER_DIR/" > /dev/null
  fi
  for class in $classes; do
    local receipt=$results/$class
    local command=(adb -s "$serial" shell am instrument -w -r -e class "org.wezterm.android.$class")
    if [ "$variant" = debug ]; then
      command+=(-e configOverrides "$(shell_quote "${WEZTERM_ANDROID_CONFIG_OVERRIDES:-}")")
    fi
    command+=(org.wezterm.android.test/androidx.test.runner.AndroidJUnitRunner)
    printf '%q ' "${command[@]}" > "$receipt.command"
    printf '\n' >> "$receipt.command"
    (exec adb -s "$serial" logcat -v time 'wezterm:V' 'WezTermSurface:V' 'WezTermSshMuxTest:V' 'WezTermInputTest:V' 'TestRunner:V' 'AndroidRuntime:E' '*:S') > "$receipt.logcat" 2>&1 &
    logcat_pid=$!
    printf '%s\n' "$logcat_pid" > "$receipt.logcat.pid"
    logcat_exit=$receipt.logcat.exit
    local status=0
    "${command[@]}" > "$receipt.instrument" 2>&1 || status=$?
    printf '%s\n' "$status" > "$receipt.exit"
    stop_logcat
    uv run --no-project python ci/android_instrument.py parse "$suite" "$class" "$receipt.instrument" "$receipt.exit" "$receipt.xml"
  done
)

case "${1:-}" in
  provision)        shift; cmd_provision "$@" ;;
  native)           shift; cmd_native "$@" ;;
  build)            shift; cmd_build "$@" ;;
  release)          shift; cmd_release "$@" ;;
  check)            shift; cmd_check "$@" ;;
  inspect)          shift; cmd_inspect "$@" ;;
  inspect-selftest) shift; cmd_inspect_selftest "$@" ;;
  install)          shift; cmd_install "$@" ;;
  test)             shift; cmd_test "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
