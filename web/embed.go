package web

import "embed"

// Dist is the built SPA (web/dist). Run `pnpm --dir web build` (or `make
// build`, which does it first) to regenerate; a placeholder index.html keeps
// `go build` working before the first web build.
//
//go:embed dist
var Dist embed.FS
