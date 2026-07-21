.PHONY: install check

# Install the release binary into ~/.cargo/bin, then restart the daemon on it
# so the running watch never lags behind what was just merged/built.
install:
	cargo install --path . --locked
	meguri daemon restart
	meguri daemon status

# Same battery as CI (run before committing).
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo nextest run
	cargo test --doc
