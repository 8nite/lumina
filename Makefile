BINARY := lumina
PKG := github.com/8nite/lumina
GOFLAGS := CGO_ENABLED=0
PREFIX ?= $(HOME)/.local

.PHONY: build dev test lint openapi-gen web-build clean install

build: web-build
	$(GOFLAGS) go build -o bin/$(BINARY) ./cmd/lumina

install: build
	install -d $(PREFIX)/bin
	install -m 0755 bin/$(BINARY) $(PREFIX)/bin/$(BINARY)
	@echo "installed to $(PREFIX)/bin/$(BINARY) (ensure it is on PATH)"

dev:
	$(GOFLAGS) go run ./cmd/lumina serve

test: web/dist
	$(GOFLAGS) go test ./...

# web/dist is gitignored; ensure the SPA exists before Go embeds it.
web/dist:
	pnpm --dir web install && pnpm --dir web build

test-m0:
	$(GOFLAGS) go test ./internal/config ./internal/rules ./internal/security ./internal/server

test-m1:
	$(GOFLAGS) go test ./internal/llm ./internal/agent ./internal/plugins ./internal/tools

test-m2:
	$(GOFLAGS) go test ./internal/kb

lint:
	go vet ./...

openapi-gen:
	go run ./cmd/lumina openapi --out web/src/api/openapi.json

web-build:
	pnpm --dir web install && pnpm --dir web build

clean:
	rm -rf bin/ web/dist/
