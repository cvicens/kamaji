.DEFAULT_GOAL := help

SERVICE      := kamaji
REMOTE       ?=
BIN_PATH     := target/release/kamajid
CLI_BIN_PATH := target/release/kamaji

.PHONY: help build release run cli watch test test-verbose \
        fmt fmt-check clippy check ci \
        clean doc \
        service-status service-logs deploy \
        release-patch release-minor release-major

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Debug build (kamajid daemon + kamaji CLI)
	cargo build --workspace

release: ## Release build (kamajid daemon + kamaji CLI)
	cargo build --release --workspace

run: ## Run the daemon (debug build)
	cargo run --bin kamajid

cli: ## Run the CLI against a running daemon (debug build), e.g. make cli ARGS="status"
	cargo run --bin kamaji -- $(ARGS)

watch: ## Rebuild and rerun the daemon on file changes (requires cargo-watch)
	cargo watch -x 'run --bin kamajid'

test: ## Run the test suite
	cargo test --workspace

test-verbose: ## Run tests with output shown
	cargo test --workspace -- --nocapture

fmt: ## Format the code
	cargo fmt

fmt-check: ## Check formatting without modifying files
	cargo fmt --check

clippy: ## Lint with clippy, warnings as errors
	cargo clippy --all-targets --workspace -- -D warnings

check: fmt-check clippy test ## Run fmt-check, clippy, and tests

ci: check ## Alias for check, matches CI gate

clean: ## Remove build artifacts
	cargo clean

doc: ## Build and open docs
	cargo doc --open --no-deps

service-status: ## Show systemd status for the deployed service (local or REMOTE=user@host)
ifdef REMOTE
	ssh $(REMOTE) sudo systemctl status $(SERVICE)
else
	sudo systemctl status $(SERVICE)
endif

service-logs: ## Tail journalctl logs for the deployed service (local or REMOTE=user@host)
ifdef REMOTE
	ssh $(REMOTE) sudo journalctl -u $(SERVICE) -f
else
	sudo journalctl -u $(SERVICE) -f
endif

deploy: release ## Copy release binaries to REMOTE=user@host, or install kamaji CLI to ~/bin if REMOTE is unset
ifdef REMOTE
	scp $(BIN_PATH) $(REMOTE):/tmp/kamajid
	scp $(CLI_BIN_PATH) $(REMOTE):/tmp/kamaji
	ssh $(REMOTE) 'sudo install -o $(SERVICE) -g $(SERVICE) -m 755 /tmp/kamajid /opt/$(SERVICE)/bin/kamajid && sudo install -o $(SERVICE) -g $(SERVICE) -m 755 /tmp/kamaji /opt/$(SERVICE)/bin/kamaji && rm /tmp/kamajid /tmp/kamaji && sudo systemctl restart $(SERVICE)'
else
	install -d $(HOME)/bin
	install -m 755 $(CLI_BIN_PATH) $(HOME)/bin/kamaji
endif

## --- Release management with cargo-release ---
## Each bumps Cargo.toml's version, runs `make ci` as a gate (see release.toml),
## commits, tags v<version>, and pushes -- never publishes to crates.io.

release-patch: ## Bump patch version (x.y.Z+1), tag, and push
	@echo "Creating patch release (x.y.Z+1)..."
	cargo release patch --execute

release-minor: ## Bump minor version (x.Y+1.0), tag, and push
	@echo "Creating minor release (x.Y+1.0)..."
	cargo release minor --execute

release-major: ## Bump major version (X+1.0.0), tag, and push
	@echo "Creating major release (X+1.0.0)..."
	cargo release major --execute
