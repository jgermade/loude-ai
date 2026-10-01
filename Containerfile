# `luu-worker`: level 3 of the sandbox ladder, in its development posture.
#
# The container's PID 1 is `luu worker`, which is what the host talks to over
# stdio. Its lifetime is therefore the session's, `--rm` and a closed stdin are
# the whole of the cleanup, and there is no way to leave one running after the
# session that owned it died. See
# RECORD/2026-09-02.the-worker-and-the-seam.completed.md. The page's terminal
# `exec`s a shell into it by name — `bash`, which the rust image carries, or
# `sh` — see RECORD/2026-10-01.a-terminal-in-the-container.completed.md.
#
# Build it, and point a session at it:
#
#   docker build -t luu-worker:dev -f Containerfile .
#   cargo run --bin luu -- tools --sandbox luu.container.toml
#
# Named `Containerfile` rather than `Dockerfile` for the same reason the runtime
# is a name rather than an integration: Podman, nerdctl and Buildah read this
# file under this name, and `docker build -f` takes it happily.
#
# **This is a development image and it is not minimal.** The design says
# "compile to a static binary (musl) → minimal image (scratch/distroless)", and
# that is wrong for a container whose entire job is running `cargo`, `rustc`,
# `git`, `rg` and `ls`: a `scratch` image has none of them, and mounting the
# host's toolchain does not help when the host is macOS/aarch64 and the guest is
# Linux. `commands = [...]` in the policy is this file's manifest, and the two
# have to agree — `luu tools` prints the gap, under `absent`.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin luu

FROM rust:1-bookworm
# The rust image already carries cargo, rustc, git and coreutils. `rg` is the
# one thing luu.toml's `commands` names that it does not, and leaving it out is
# how you get to watch `absent` do its job.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ripgrep \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/luu /usr/local/bin/luu

# No ENTRYPOINT on purpose. The host builds the whole command line —
# `luu worker --command …` — and an entrypoint would prepend to it.
#
# No USER either: the base directory is bind-mounted at its host path, so the
# worker runs as whoever started the session (`--user uid:gid`, or `--uid`/
# `--gid` under Apple's runtime) and leaves the checkout owned by them.
CMD ["luu", "worker"]
