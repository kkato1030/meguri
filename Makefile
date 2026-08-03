.PHONY: install check

install:
	cargo install --path . --locked

# Same battery as CI (run before committing).
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo nextest run
	cargo test --doc
