# The four things a person types, and nothing else.
#
# Every target here is a `cargo` line someone would otherwise have to remember,
# and AGENTS.md stays the place that says *why* each of them is the command it
# is. If a target ever needs a paragraph, the paragraph belongs there and the
# target belongs here.
#
# The VS Code extension (editors/vscode) is TypeScript and is part of `install`
# and `build` when npm is on the PATH, and skipped with a line saying so when it
# is not — a Rust checkout that cannot build the editor surface is still a
# working checkout.

CARGO ?= cargo
EXTENSION := editors/vscode
# The debug UI's *optional* browser dependencies — today that is Monaco, which
# the General settings offer only where it is installed. A checkout without npm
# gets a page that draws every file itself, which is the default either way.
WEBUI := web
BIND ?= 127.0.0.1:7878

.DEFAULT_GOAL := help
.PHONY: help install test build up fmt lint smoke

help:
	@echo "make install   fetch dependencies: Rust, the VS Code extension, the UI's optional ones"
	@echo "make test      cargo test --workspace, and the probes that need no model"
	@echo "make build     release binary, and the extension if npm is here"
	@echo "make up        the debug UI and the agent protocol on $(BIND)"
	@echo "make smoke     the page driven through the gate, against a live mock server"
	@echo ""
	@echo "make fmt       cargo fmt --all"
	@echo "make lint      what CI runs: fmt --check and clippy with -D warnings"

install:
	$(CARGO) fetch
	@if command -v npm >/dev/null 2>&1; then \
		echo "==> npm ci in $(EXTENSION)"; \
		cd $(EXTENSION) && npm ci; \
	else \
		echo "==> no npm on PATH: skipping $(EXTENSION)"; \
	fi
	@if command -v npm >/dev/null 2>&1; then \
		echo "==> npm install in $(WEBUI) (optional: Monaco)"; \
		cd $(WEBUI) && npm install --no-audit --no-fund; \
	else \
		echo "==> no npm on PATH: the debug UI keeps its own viewer"; \
	fi

# The whole workspace, which includes the selection probe — 38 questions scored
# against scripts/tasks/map-order-probe.key with no model on the machine.
test:
	$(CARGO) test --workspace

build:
	$(CARGO) build --release --workspace
	@if command -v npm >/dev/null 2>&1; then \
		echo "==> tsc in $(EXTENSION)"; \
		cd $(EXTENSION) && npm run compile; \
	else \
		echo "==> no npm on PATH: skipping $(EXTENSION)"; \
	fi

# Loopback, and no token: `serve` refuses any other address without
# --auth-token-file, because /ws carries job approval. Sessions are cached in
# the state directory, so a restart does not lose the conversation.
up:
	$(CARGO) run --bin luu -- serve --bind $(BIND)

# The page, clicked: a prompt, the gate, an amendment, Approve, the tool call
# and the fold — against `luu serve` on the mock, which the spec starts itself.
# Needs node and a built binary, which is why it is not part of `make test`.
smoke: build
	cd tests/smoke && npm ci && npx playwright install chromium && \
		LUU_BIN=target/release/luu npx playwright test -c gate.config.js

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) fmt --all --check
	RUSTFLAGS="-D warnings" $(CARGO) clippy --workspace --all-targets
