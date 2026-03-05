.PHONY: check clippy test fmt lint-css lint-html lint lint-all kill

check:
	cargo check --workspace

clippy:
	cargo clippy --workspace -- -D warnings

test:
	cargo test --workspace

fmt:
	cargo fmt --all -- --check

lint-css:
	npx stylelint 'ferret-web/static/**/*.css'

lint-html:
	djlint ferret-web/templates/ --lint

lint: clippy fmt lint-css lint-html

lint-all: check lint test

kill:
	@pids=$$(ps aux | grep 'ferret daemon-start --repo' | grep -v grep | awk '{ print $$2 }'); \
	if [ -n "$$pids" ]; then kill $$pids; echo "Killed: $$pids"; else echo "No ferret daemons running"; fi

