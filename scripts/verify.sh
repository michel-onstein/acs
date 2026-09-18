#!/bin/sh
# Everything that must pass before a change lands: format, lint, tests, docs.
set -eu
cd "$(dirname "$0")/.."

echo '== cargo fmt'
cargo fmt --all -- --check
echo '== cargo clippy'
cargo clippy --workspace --all-targets -- -D warnings
echo '== cargo test'
cargo test --workspace
echo '== markdownlint'
npx -y markdownlint-cli2 '**/*.md' '#target' '#.claude'
echo '== ok'
