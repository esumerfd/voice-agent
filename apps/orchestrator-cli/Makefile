.PHONY: test build run

test:
	cargo test -p orchestrator-cli --target-dir target

build:
	cargo build -p orchestrator-cli --target-dir target

run:
	cargo run -p orchestrator-cli --target-dir target -- list
