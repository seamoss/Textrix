#!/bin/sh
# Textrix on Linux, in Docker (packaging/docker/Dockerfile).
#
#   scripts/linux.sh test            build it and run the tests on Linux
#   scripts/linux.sh [run] [args]    run Textrix there (args go to it)
#   scripts/linux.sh shell           a shell there, in the same session
#
# Running uses your dotfile (TEXTRIX_ENV_FILE, else the repo's .textrix,
# else ~/.textrix/.env), mounted read-only, never copied into the image.
# Its state, keyring included, lives in the textrix-linux volume, so it
# signs in once (as a new session on your account) and restores after;
# `docker volume rm textrix-linux` starts over.
set -eu
cd "$(dirname "$0")/.."
image=textrix-linux
build() { docker build "$@" -f packaging/docker/Dockerfile .; }

command=${1:-run}
[ $# -gt 0 ] && shift
case "$command" in
    test) build --target test ;;
    run | shell)
        build -q -t "$image" >/dev/null
        dotfile=${TEXTRIX_ENV_FILE:-}
        [ -z "$dotfile" ] && [ -f .textrix ] && dotfile=$PWD/.textrix
        [ -z "$dotfile" ] && dotfile=$HOME/.textrix/.env
        if [ ! -f "$dotfile" ]; then
            echo "no dotfile at $dotfile (TEXTRIX_ENV_FILE picks one)" >&2
            exit 1
        fi
        if [ "$command" = shell ]; then set -- bash; else set -- textrix --env /run/secrets/textrix.env "$@"; fi
        exec docker run --rm -it \
            -v textrix-linux:/home/textrix \
            -v "$dotfile:/run/secrets/textrix.env:ro" \
            "$image" "$@"
        ;;
    *)
        echo "usage: scripts/linux.sh [test | run [textrix args] | shell]" >&2
        exit 2
        ;;
esac
