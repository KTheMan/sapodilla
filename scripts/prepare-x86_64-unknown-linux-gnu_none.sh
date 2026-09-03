#!/bin/sh

set -exu

sudo apt-get update
sudo apt-get install --yes libdbus-1-dev pkg-config

rustup toolchain install stable
rustup component add clippy
