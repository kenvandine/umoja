# Umoja developer entry points.
#
#   make                 build the workspace (release)
#   make test            run the headless unit tests
#   make vendor          vendor crates + the embedded Lemonade (needed for .debs)
#   make deb             build binary packages locally
#   make source-package  build a signed-ready source package for the PPA
#   make nested          run umoja-mir in a window inside your current session

CARGO ?= cargo
LEMONADE_VERSION ?= 11.9.0
LEMONADE_ARCHES ?= x64 arm64
LEMONADE_BASE := https://github.com/lemonade-sdk/lemonade/releases/download/v$(LEMONADE_VERSION)
VENDOR := vendor

.PHONY: all build test check vendor vendor-cargo vendor-lemonade deb source-package nested greeter-preview greeter-nested clean

all: build

build:
	$(CARGO) build --release --workspace

check:
	$(CARGO) check --workspace --all-targets

# GTK crates need a display; these cover the policy/logic crates.
test:
	$(CARGO) test -p umoja-common -p umoja-mir -p umoja-portal -p umoja-panel -p umoja-files -p umoja-mcp

vendor: vendor-cargo vendor-lemonade

# cargo-vendor-filterer (if installed) drops Windows/macOS-only crates,
# which shrinks the PPA source upload by a few hundred MB.
vendor-cargo:
	@if command -v cargo-vendor-filterer >/dev/null 2>&1; then \
	  $(CARGO) vendor-filterer --platform=x86_64-unknown-linux-gnu \
	    --platform=aarch64-unknown-linux-gnu $(VENDOR)/cargo > /dev/null; \
	else \
	  $(CARGO) vendor --locked $(VENDOR)/cargo > /dev/null; \
	fi
	@echo "vendored crates in $(VENDOR)/cargo"

# Download the upstream "embeddable" Lemonade build for each architecture,
# verify it against lemonade.sha256, and unpack it to vendor/lemonade/<arch>.
vendor-lemonade:
	@set -e; for arch in $(LEMONADE_ARCHES); do \
	  tarball=lemonade-embeddable-$(LEMONADE_VERSION)-ubuntu-$$arch.tar.gz; \
	  mkdir -p $(VENDOR)/lemonade/dl; \
	  if [ ! -f $(VENDOR)/lemonade/dl/$$tarball ]; then \
	    echo "downloading $$tarball"; \
	    curl -fsSL -o $(VENDOR)/lemonade/dl/$$tarball $(LEMONADE_BASE)/$$tarball; \
	  fi; \
	  (cd $(VENDOR)/lemonade/dl && grep " $$tarball\$$" ../../../lemonade.sha256 | sha256sum -c -); \
	  rm -rf $(VENDOR)/lemonade/$$arch; mkdir -p $(VENDOR)/lemonade/$$arch; \
	  tar -xzf $(VENDOR)/lemonade/dl/$$tarball -C $(VENDOR)/lemonade/$$arch --strip-components=1; \
	  test -x $(VENDOR)/lemonade/$$arch/lemond; \
	done
	rm -rf $(VENDOR)/lemonade/dl

deb: vendor
	dpkg-buildpackage -b -us -uc

source-package: vendor
	dpkg-buildpackage -S -sa -d -us -uc

# Mir can run as a Wayland client of your existing session for development.
nested: build
	WAYLAND_DISPLAY=$${WAYLAND_DISPLAY:-wayland-0} UMOJA_NO_SESSION_TARGET=1 \
	  ./target/release/umoja-mir --platform-display-libs=mir:wayland --wayland-host=$${WAYLAND_DISPLAY:-wayland-0}

greeter-preview:
	$(CARGO) run -p umoja-greeter -- --preview

# The login screen as LightDM runs it (umoja-mir's greeter mode), nested.
greeter-nested:
	$(CARGO) build -p umoja-mir -p umoja-greeter
	WAYLAND_DISPLAY=$${WAYLAND_DISPLAY:-wayland-0} \
	  UMOJA_GREETER="$(CURDIR)/target/debug/umoja-greeter --preview" \
	  ./target/debug/umoja-mir --platform-display-libs=mir:wayland --wayland-host=$${WAYLAND_DISPLAY:-wayland-0}

clean:
	$(CARGO) clean
	rm -rf $(VENDOR)/cargo $(VENDOR)/lemonade
