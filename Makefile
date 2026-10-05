IMAGE ?= lrc-sync

.PHONY: build check fmt clippy test verify update

build:
	docker build -t $(IMAGE) .

check:
	cargo check --locked --all-targets

fmt:
	cargo fmt --check

clippy:
	cargo clippy --locked --all-targets -- -D warnings

test:
	cargo test --locked

verify: fmt clippy test

update:
	cargo update
	$(MAKE) verify
