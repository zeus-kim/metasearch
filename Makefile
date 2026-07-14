.PHONY: run build test clippy

run:
	./scripts/run.sh

build:
	cargo build --bin metasearch

test:
	cargo test

clippy:
	cargo clippy --all-targets -- -D warnings
