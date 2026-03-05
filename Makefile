.PHONY: check clippy test fmt lint-css lint-html lint lint-all

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
