IMAGE ?= lrc-sync

.PHONY: build check fmt clippy test

build:
	docker build -t $(IMAGE) .

check:
	cargo check --locked

fmt:
	cargo fmt --check

clippy:
	cargo clippy --locked -- -D warnings

test:
	cargo test --locked

