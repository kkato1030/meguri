.PHONY: install check

install:
	cargo install --path . --locked

# Same battery as CI (run before committing).
# v2 は bin のみのクレートなので doc テスト工程は無い(`cargo test --doc` は
# lib ターゲット不在でエラーになる)。lib を持つ日が来たら戻す。
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo nextest run
