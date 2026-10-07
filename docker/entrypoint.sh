#!/bin/sh
# roc agent entrypoint: register the (arbitrary) host uid/gid so tools that
# look up the current user (git, ssh, npm) work, then exec the agent.
set -e

uid="$(id -u)"
gid="$(id -g)"
home="${HOME:-/home/roc}"

if ! getent group "$gid" >/dev/null 2>&1 && [ -w /etc/group ]; then
    echo "roc:x:${gid}:" >> /etc/group
fi
if ! getent passwd "$uid" >/dev/null 2>&1 && [ -w /etc/passwd ]; then
    echo "roc:x:${uid}:${gid}:roc agent:${home}:/bin/bash" >> /etc/passwd
fi

mkdir -p "$home/.cache" "$home/.local/bin" "$home/.npm-global" 2>/dev/null || true

# Git refuses to work in directories owned by "someone else"; the 1:1 mounts
# are yours, so trust them. (Env-based so a read-only ~/.gitconfig still works.)
if [ -z "${GIT_CONFIG_COUNT:-}" ]; then
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'
fi

exec "$@"
