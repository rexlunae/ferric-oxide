#!/usr/bin/env bash
# Usage: annotate-build-errors.sh LOG TITLE
#
# Prints one GitHub Actions error annotation, titled TITLE, that explains why
# the build whose output is LOG failed. An annotation keeps only its first few
# KiB, so instead of the end of the log (often pages of strip and copy lines
# after the error) it carries the lines that report an error:
#
#   cook <recipe> - failed     the cookbook giving up on a recipe
#   Failed to run ...          the command the cookbook ran
#   error: / error[E...]:      compiler, Cargo and tool errors, at the start
#                              of a line or after a "file:line: " prefix
#   make: *** / make[N]: ***   make stopping
#
# each with the CONTEXT_BEFORE lines before it and CONTEXT_AFTER lines after
# it, overlapping excerpts merged and gaps marked with "--". Excerpts are kept
# in log order up to MAX_BYTES, so the first error, usually the cause, is
# always there. A log with no such line falls back to its last TAIL_LINES
# lines. Colour codes and carriage-return progress output are removed first.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 LOG TITLE" >&2
    exit 2
fi
log="$1"
title="$2"
before="${CONTEXT_BEFORE:-5}"
after="${CONTEXT_AFTER:-2}"
max_bytes="${MAX_BYTES:-3500}"
tail_lines="${TAIL_LINES:-40}"
# A single line longer than this (a long compiler command) is cut, so one line
# cannot use up the annotation.
max_line=400
# "error:" counts at the start of a line (rustc, Cargo) or after a "file:line: "
# prefix (C compilers, linkers), not inside quoted source that rustc prints.
error_re='cook .* - failed|Failed to run|(^|: )(fatal )?error(\[E[0-9]+\])?:|make(\[[0-9]+\])?: \*\*\*'

clean() {
    # A progress bar redraws one line with CRs; keep what the terminal last showed.
    sed -e 's/\x1b\[[0-9;?]*[A-Za-z]//g' -e 's/^.*\r\(.\)/\1/' -e 's/\r$//' "$log" |
        awk -v max="$max_line" '{ if (length($0) > max) $0 = substr($0, 1, max) " [...]"; print }'
}

excerpt() {
    # Through the environment: -v would turn the pattern's \[ and \* into [ and *.
    clean | ERROR_RE="$error_re" awk -v before="$before" -v after="$after" '
        function emit(n, text) {
            if (last && n > last + 1) print "--"
            print text
            last = n
        }
        BEGIN { re = ENVIRON["ERROR_RE"] }
        {
            ring[NR % (before + 1)] = $0
            if ($0 ~ re) {
                start = NR - before
                if (start <= last) start = last + 1
                if (start < 1) start = 1
                for (i = start; i < NR; i++) emit(i, ring[i % (before + 1)])
                emit(NR, $0)
                upto = NR + after
            } else if (NR <= upto) {
                emit(NR, $0)
            }
        }'
}

body="$(excerpt)"
if [ -n "$body" ]; then
    heading="Error lines from the build log, with context:"
else
    heading="No error line found; the last $tail_lines lines of the build log:"
    body="$(clean | tail -n "$tail_lines")"
fi

# Whole lines, in order, until the budget runs out.
body="$(printf '%s\n' "$body" | awk -v max="$max_bytes" '
    { size += length($0) + 1 }
    size > max { cut++; next }
    { print }
    END { if (cut) printf "[... %d more lines; see the job log]\n", cut }')"

# Workflow command encoding: data escapes % CR LF; properties also : and ,.
escape_data() { sed -e 's/%/%25/g' | awk '{ printf "%s%s", sep, $0; sep = "%0A" }'; }
escape_property() { printf '%s' "$1" | sed -e 's/%/%25/g' -e 's/:/%3A/g' -e 's/,/%2C/g'; }

printf '::error title=%s::%s\n' "$(escape_property "$title")" \
    "$(printf '%s\n%s\n' "$heading" "$body" | escape_data)"
