#!/bin/sh
# Stand-in for gh. It never execs the real gh, so it cannot write to GitHub.
# `gh pr review` sleeps 5 seconds and exits 0. Other calls print a one-PR
# fixture or nothing, which is enough to open a file list for a local check.
# Set FAKE_GH_LOG to a path to record each invocation.

if [ -n "${FAKE_GH_LOG:-}" ]; then
    printf '%s\n' "$*" >> "$FAKE_GH_LOG"
fi

if [ "$1" = "pr" ] && [ "$2" = "review" ]; then
    sleep 5
    exit 0
fi

if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
    printf '%s\n' 'example/demo'
    exit 0
fi

endpoint=
for arg in "$@"; do
    case "$arg" in
        repos/*) endpoint=$arg ;;
    esac
done

case "$endpoint" in
    */files*)
        printf '%s\n' '[[{"filename":"src/main.rs","status":"modified","additions":1,"deletions":0,"patch":"@@ -1 +1 @@\n-old\n+new\n"}]]'
        exit 0
        ;;
    */pulls/*)
        case "$endpoint" in
            */comments*|*/reviews*) exit 0 ;;
        esac
        printf '%s\n' '{"number":1,"title":"demo","body":null,"state":"open","head":{"ref":"demo","sha":"abc"},"base":{"ref":"main","sha":"def"},"user":{"login":"octo"},"updated_at":"2026-11-02T00:00:00Z"}'
        exit 0
        ;;
esac

exit 0
