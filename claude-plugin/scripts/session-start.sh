#!/bin/sh
# SessionStart hook of the rtk Claude Code plugin. It tells Claude how rtk-condensed
# command output reads (awareness.md), and tells the user, in a systemMessage that
# Claude does not see, when the rtk on PATH cannot condense output. It always exits 0.
#
# The rewrite itself is not here: the guardrails plugin's `rtk` option runs
# `rtk hook claude`, and a second rewrite hook would race it.

root="${0%/*}/.."

# json FILE: FILE's text as a JSON string body (no surrounding quotes, no final newline).
json() {
	awk 'BEGIN { ORS = "" }
	{
		gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t"); gsub(/\r/, "\\r")
		if (NR > 1) printf "\\n"
		print
	}' "$1"
}

reply() { # reply MESSAGE WITH_CONTEXT
	printf '{'
	[ -n "$1" ] && printf '"systemMessage":"%s",' "$1"
	printf '"hookSpecificOutput":{"hookEventName":"SessionStart"'
	[ "$2" = yes ] && printf ',"additionalContext":"%s"' "$(json "$root/awareness.md")"
	printf '}}\n'
	exit 0
}

if ! command -v rtk >/dev/null 2>&1; then
	reply "rtk plugin: rtk is not on PATH, so command output is not condensed. Install https://github.com/jj-scoll/rtk." no
fi
if ! rtk gain >/dev/null 2>&1; then
	reply "rtk plugin: the rtk on PATH does not answer \`rtk gain\`; it may be another tool named rtk." no
fi
# Only the fork has the duckdb filter, so this tells the fork from upstream rtk.
if ! rtk duckdb --help >/dev/null 2>&1; then
	reply "rtk plugin: the rtk on PATH is upstream rtk, not https://github.com/jj-scoll/rtk; the fork's extra filters are missing." yes
fi
reply "" yes
