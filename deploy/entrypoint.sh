#!/bin/sh
# First start: a `config.toml` that says how this server is reached, and the
# token it names — written once, and then the person's to edit. Every later
# start leaves both alone. Arguments go to `luu serve`.
set -eu
state="${LUU_HOME:-/state}"

# Granted in the policy, and a granted path has to exist: on an empty volume
# it does not until something makes it.
mkdir -p "${CARGO_HOME:-$state/cargo}"

if [ ! -f "$state/config.toml" ]; then
  cat > "$state/config.toml" <<TOML
# Written by the container on its first start. Yours to edit: see
# RECORD/2026-10-07.a-public-luu.WIP.md for every key.
[server]
exposure = "private"
bind = "0.0.0.0:7878"
token-file = "$state/token"
TOML
fi

# Made here rather than baked into the image, so no two containers share one.
# Read it with `cat $state/token`; it is never printed.
luu token "$state/token" --if-missing

exec luu serve --sandbox /etc/luu/luu.toml "$@"
