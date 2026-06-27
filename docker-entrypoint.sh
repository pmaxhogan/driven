#!/bin/sh
# Entrypoint for the public Driven headless image.
#
#   docker run --rm IMAGE                         -> driven-cli --help
#   docker run --rm IMAGE driven-cli ...          -> run the CLI
#   docker run --rm IMAGE driven-chaos ...        -> run the chaos harness
#   docker run --rm IMAGE chaos-soak [--duration 6h]
#         -> replicate the justfile `chaos-soak` recipe exactly:
#              $env:DRIVEN_CHAOS_SOAK="1"; driven-chaos run-all --hermetic
#              driven-chaos fuzz {{args}}
#            The justfile runs each recipe line in its OWN shell, so the soak
#            env is set for run-all only and the fuzz step does NOT inherit it.
#
# Any other first argument is treated as a command and exec'd directly (so
# `docker run IMAGE sh` etc. still works for debugging).

set -e

case "$1" in
    "")
        exec driven-cli --help
        ;;
    chaos-soak)
        shift
        # Match the justfile recipe default: args="--duration 30m".
        if [ "$#" -eq 0 ]; then
            set -- --duration 30m
        fi
        # Line 1 of the recipe: soak env set for the full hermetic sweep so the
        # soak-gated massive-input rows (million-files-nested, tiny-files-100k)
        # run. `set -e` makes a failure here abort before fuzz, like `just` does.
        DRIVEN_CHAOS_SOAK=1 driven-chaos run-all --hermetic
        # Line 2: the seeded fuzz soak, WITHOUT DRIVEN_CHAOS_SOAK in its env.
        exec driven-chaos fuzz "$@"
        ;;
    *)
        exec "$@"
        ;;
esac
