BINARY := lumina
PKG := github.com/8nite/lumina
GOFLAGS := CGO_ENABLED=0
PREFIX ?= $(HOME)/.local
export PATH := $(HOME)/.cargo/bin:$(PATH)

.PHONY: build dev test lint openapi-gen web-build clean install stop rust-build rust-test rust-release rust-lint build-go test-go

# Default: Rust single binary (Go sources remain for reference; use build-go/test-go).
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

build-go: web-build
	$(GOFLAGS) go build -o bin/$(BINARY) ./cmd/lumina

test-go: web/dist
	$(GOFLAGS) go test ./...

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

test-m0:
	$(GOFLAGS) go test ./internal/config ./internal/rules ./internal/security ./internal/server

test-m1:
	$(GOFLAGS) go test ./internal/llm ./internal/agent ./internal/plugins ./internal/tools

test-m2:
	$(GOFLAGS) go test ./internal/kb

lint: rust-lint

lint-go:
	go vet ./...

openapi-gen:
	cargo run --quiet -- openapi --out web/src/api/openapi.json

openapi-gen-go:
	go run ./cmd/lumina openapi --out web/src/api/openapi.json

web-build:
	pnpm --dir web install && pnpm --dir web build

clean:
	rm -rf bin/ web/dist/
