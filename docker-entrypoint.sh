#!/bin/sh
set -e

DATA_DIR="${GITAI_DATA_DIR:-/data}"
CONFIG_FILE="${GITAI_CONFIG:-$DATA_DIR/gitai.toml}"

# Ensure required directories exist
mkdir -p "$DATA_DIR" "$DATA_DIR/prompts" "$DATA_DIR/work"

# Initialize prompts if not already present
if [ -z "$(ls -A "$DATA_DIR/prompts" 2>/dev/null)" ]; then
    if [ -d "/app/prompts" ]; then
        cp -r /app/prompts/* "$DATA_DIR/prompts/" 2>/dev/null || true
    fi
fi

# Initialize config file if not present
if [ ! -f "$CONFIG_FILE" ]; then
    if [ -f "/etc/gitai/gitai.toml" ]; then
        cp "/etc/gitai/gitai.toml" "$CONFIG_FILE"
    elif [ -f "/app/deploy/docker-gitai.toml" ]; then
        cp "/app/deploy/docker-gitai.toml" "$CONFIG_FILE"
    elif [ -f "/app/gitai.example.toml" ]; then
        cp "/app/gitai.example.toml" "$CONFIG_FILE"
    fi
fi

# Handle subcommands and default execution
if [ $# -eq 0 ]; then
    exec gitai --config "$CONFIG_FILE" serve
fi

case "$1" in
    serve|doctor|init)
        cmd="$1"
        shift
        exec gitai --config "$CONFIG_FILE" "$cmd" "$@"
        ;;
    run)
        shift
        exec gitai --config "$CONFIG_FILE" run "$@"
        ;;
    gitai)
        shift
        # If user did not pass --config, default to $CONFIG_FILE
        has_config=0
        for arg in "$@"; do
            if [ "$arg" = "-c" ] || [ "$arg" = "--config" ]; then
                has_config=1
                break
            fi
        done
        if [ $has_config -eq 0 ]; then
            exec gitai --config "$CONFIG_FILE" "$@"
        else
            exec gitai "$@"
        fi
        ;;
    -*)
        exec gitai --config "$CONFIG_FILE" serve "$@"
        ;;
    *)
        exec "$@"
        ;;
esac
