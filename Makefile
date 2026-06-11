IMAGE ?= lrcget-cli

.PHONY: build check

build:
	docker build -t $(IMAGE) .

check:
	python3 -m py_compile sync_lyrics.py

