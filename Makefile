CARGO ?= cargo
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BINARY := target/release/gbonds

.PHONY: all build install uninstall test fmt fmt-check check clean

all: build

build:
	$(CARGO) build --release --locked

install: build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 755 "$(BINARY)" "$(DESTDIR)$(BINDIR)/gbonds"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/gbonds"

test:
	$(CARGO) test --locked

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt --check

check: fmt-check test

clean:
	$(CARGO) clean
