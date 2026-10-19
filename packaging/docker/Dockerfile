# syntax=docker/dockerfile:1
# Textrix on Linux, for testing (scripts/linux.sh runs it): built in a Debian
# container, with what a Linux desktop provides around it, a D-Bus session
# with GNOME Keyring for the Secret Service (where Textrix keeps its store
# key), notify-send, and the audio players Textrix looks for.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Without the release profile's whole-program optimisation: much quicker to
# build, and it's for testing.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    cargo build --release --locked && cp target/release/textrix /usr/local/bin/textrix

# `docker build --target test`: the test suite, on Linux.
FROM build AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo test --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates dbus dbus-x11 gnome-keyring libsecret-tools libnotify-bin \
        pulseaudio-utils alsa-utils xdg-utils locales \
    && sed -i 's/# en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen && locale-gen \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --shell /bin/bash textrix
COPY --from=build /usr/local/bin/textrix /usr/local/bin/textrix
COPY packaging/docker/session.sh /usr/local/bin/textrix-session
USER textrix
WORKDIR /home/textrix
ENV LANG=en_US.UTF-8 TERM=xterm-256color
ENTRYPOINT ["textrix-session"]
CMD ["textrix"]
