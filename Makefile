prefix ?= /usr
bindir ?= $(prefix)/bin
libexecdir ?= $(prefix)/libexec
DESTDIR ?=
INSTALL ?= install

CARGO ?= cargo
RUSTC ?= rustc
CARGO_TARGET ?= $(shell $(RUSTC) -vV | sed -n 's/^host: //p')
CARGO_TARGET_DIR ?= target
CARGO_BUILD_FLAGS ?= --locked
XTASK_FLAGS ?=
AUTOINIT ?= $(CARGO_TARGET_DIR)/$(CARGO_TARGET)/release/autoinit
AUTOINIT_NMSTATE ?= $(CARGO_TARGET_DIR)/$(CARGO_TARGET)/release/autoinit-nmstate
SKIP_BUILD ?=

.PHONY: all build check install clean package package-srpm

all: build

build:
	RUSTFLAGS="$(RUSTFLAGS) -C target-feature=+crt-static" $(CARGO) build --release \
		--no-default-features --bin autoinit \
		--target $(CARGO_TARGET) --target-dir "$(CARGO_TARGET_DIR)" $(CARGO_BUILD_FLAGS)
	RUSTFLAGS="$(RUSTFLAGS) -C target-feature=+crt-static" $(CARGO) build --release \
		--no-default-features --features nmstate --bin autoinit-nmstate \
		--target $(CARGO_TARGET) --target-dir "$(CARGO_TARGET_DIR)" $(CARGO_BUILD_FLAGS)

check:
	RUSTFLAGS="$(RUSTFLAGS) -C target-feature=+crt-static" $(CARGO) test \
		--no-default-features --lib \
		--target $(CARGO_TARGET) --target-dir "$(CARGO_TARGET_DIR)" $(CARGO_BUILD_FLAGS)
	RUSTFLAGS="$(RUSTFLAGS) -C target-feature=+crt-static" $(CARGO) test \
		--no-default-features --features nmstate --lib \
		--target $(CARGO_TARGET) --target-dir "$(CARGO_TARGET_DIR)" $(CARGO_BUILD_FLAGS)

ifneq ($(SKIP_BUILD),1)
install: build
endif
install:
	$(INSTALL) -D -m 0755 "$(AUTOINIT)" "$(DESTDIR)$(libexecdir)/autoinit"
	$(INSTALL) -D -m 0755 "$(AUTOINIT_NMSTATE)" "$(DESTDIR)$(libexecdir)/autoinit-nmstate"
	$(INSTALL) -D -m 0755 mkautoinitrd "$(DESTDIR)$(bindir)/mkautoinitrd"
	python3 -c 'import pathlib, re, sys; \
		path = pathlib.Path(sys.argv[1]); \
		text, count = re.subn(r"^DEFAULT_INIT = .*$$", lambda _: "DEFAULT_INIT = " + repr(sys.argv[2]), path.read_text(), flags=re.M); \
		assert count == 1, "expected exactly one DEFAULT_INIT definition"; \
		path.write_text(text)' \
		"$(DESTDIR)$(bindir)/mkautoinitrd" "$(libexecdir)/autoinit"

clean:
	$(CARGO) clean --target-dir "$(CARGO_TARGET_DIR)"

package package-srpm:
	$(CARGO) xtask $@ $(XTASK_FLAGS)
