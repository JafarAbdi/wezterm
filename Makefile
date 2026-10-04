.PHONY: all fmt build check test docs servedocs android-provision android-native android-build android-check android-inspect android-inspect-selftest android-install android-test android-release android-release-inspect

all: build

test:
	cargo nextest run
	cargo nextest run -p wezterm-escape-parser # no_std by default

check:
	cargo check
	cargo check -p wezterm-escape-parser
	cargo check -p wezterm-cell
	cargo check -p wezterm-surface
	cargo check -p wezterm-ssh

build:
	cargo build $(BUILD_OPTS) -p wezterm
	cargo build $(BUILD_OPTS) -p wezterm-gui
	cargo build $(BUILD_OPTS) -p wezterm-mux-server
	cargo build $(BUILD_OPTS) -p strip-ansi-escapes

fmt:
	cargo +nightly fmt

docs:
	ci/build-docs.sh

servedocs:
	ci/build-docs.sh serve

ABI ?=
VARIANT ?= debug
SUITE ?= native-load

android-provision:
	ci/android.sh provision

android-native:
	ci/android.sh native "$(ABI)" "$(VARIANT)"

android-build:
	ci/android.sh build

android-release:
	ci/android.sh release

android-release-inspect:
	ci/android.sh inspect release

android-check:
	ci/android.sh check "$(ABI)" "$(VARIANT)"

android-inspect:
	ci/android.sh inspect "$(VARIANT)"

android-inspect-selftest:
	ci/android.sh inspect-selftest "$(VARIANT)"

android-install:
	ci/android.sh install $(SERIAL) "$(VARIANT)"

android-test:
	ci/android.sh test $(SERIAL) $(SUITE)
