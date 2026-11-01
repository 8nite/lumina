BINARY := lumina
PREFIX ?= $(HOME)/.local
export PATH := $(HOME)/.cargo/bin:$(PATH)

.PHONY: build dev test lint openapi-gen web-build clean install stop rust-build rust-test rust-release rust-lint

# Rust-only on this branch (Go tree deleted 2026-11-01; reference lives on main).
build: rust-build

test: rust-test

rust-build: web/dist
	cargo build
	mkdir -p bin
	cp -f target/debug/$(BINARY) bin/$(BINARY)

rust-test:
	cargo test

rust-release: web/dist
	cargo build --release
	mkdir -p bin
	cp -f target/release/$(BINARY) bin/$(BINARY)

rust-lint:
	cargo clippy --tests

stop:
	@-pkill -f "lumina serve" 2>/dev/null && echo "stopped running 'lumina serve'" || echo "no running 'lumina serve' found"

install: build
	@$(MAKE) stop
	install -d $(PREFIX)/bin
	install -m 0755 bin/$(BINARY) $(PREFIX)/bin/$(BINARY)
	@echo "installed to $(PREFIX)/bin/$(BINARY) (ensure it is on PATH)"
	@echo "shut down the previous lumina server; restart with: lumina serve (or just run lumina)"

dev:
	cargo run -- serve

# web/dist is gitignored; ensure the SPA exists before the daemon embeds it.
web/dist:
	pnpm --dir web install && pnpm --dir web build

lint: rust-lint

openapi-gen:
	cargo run --quiet -- openapi --out web/src/api/openapi.json

web-build:
	pnpm --dir web install && pnpm --dir web build

clean:
	rm -rf bin/ web/dist/
