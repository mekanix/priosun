#!/bin/sh

BINDIR=$(dirname $0)
cd "${BINDIR}/.."

cargo build --workspace
mdo env PRIOSUN_PATH=. cargo run --bin priosund -- --no-daemon $@
