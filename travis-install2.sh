#!/bin/bash
TARPAULIN_VERSION=${TARPAULIN_VERSION:-0.6.6-1}
RUST_VERSION=$(rustc -V | cut -d' ' -f 2)

if curl -L --fail --output cargo-tarpaulin.tar.gz https://github.com/AnderEnder/tarpaulin/releases/download/${TARPAULIN_VERSION}/cargo-tarpaulin-${RUST_VERSION}-${TARPAULIN_VERSION}-travis.tar.gz; then
    tar xvz -C $HOME/.cargo/bin cargo-tarpaulin.tar.gz
else
    RUSTFLAGS="--cfg procmacro2_semver_exempt" cargo install cargo-tarpaulin
fi
