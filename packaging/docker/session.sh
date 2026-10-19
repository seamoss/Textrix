#!/bin/sh
# A desktop session in miniature: D-Bus, and GNOME Keyring unlocked for the
# Secret Service, so Textrix keeps its store key the way it would on a Linux
# desktop. The keyring's password only guards this container's volume.
set -eu
exec dbus-run-session -- sh -c '
    printf textrix | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1
    exec "$@"' textrix-session "$@"
