#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Download the pinned packages listed in vm/SOURCES, verify their sha256,
# and unpack each into vm/.cache/<name>/.
#
# Any failure prints one line of the form
#   FETCH step=<step> name=<name> expected="..." got="..."
# and exits non-zero.

set -euo pipefail

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$VM_DIR/.cache"
DL="$CACHE/dl"

fail() { # step name expected got
	printf 'FETCH step=%s name=%s expected="%s" got="%s"\n' "$1" "$2" "$3" "$4" >&2
	exit 1
}

for tool in curl sha256sum dpkg-deb; do
	command -v "$tool" >/dev/null || fail tool "$tool" "$tool on PATH" "not found"
done

mkdir -p "$DL"

while read -r name sha url; do
	case "$name" in '' | '#'*) continue ;; esac
	[ -n "$sha" ] && [ -n "$url" ] || fail parse "$name" "<name> <sha256> <url>" "$name $sha $url"

	deb="$DL/$name.deb"
	if [ -f "$deb" ] && [ "$(sha256sum "$deb" | cut -d' ' -f1)" = "$sha" ]; then
		echo "fetch: $name: cached"
	else
		echo "fetch: $name: downloading $url"
		tmp="$deb.part"
		rm -f "$tmp"
		curl -fSL --retry 3 -o "$tmp" "$url" || fail download "$name" "HTTP 200 from $url" "curl exit $?"
		got="$(sha256sum "$tmp" | cut -d' ' -f1)"
		if [ "$got" != "$sha" ]; then
			rm -f "$tmp"
			fail verify "$name" "$sha" "$got"
		fi
		mv "$tmp" "$deb"
	fi

	# Unpack into a fresh directory; the stamp records which hash it came from.
	out="$CACHE/$name"
	if [ "$(cat "$out/.sha256" 2>/dev/null || true)" != "$sha" ]; then
		rm -rf "$out"
		mkdir -p "$out"
		dpkg-deb -x "$deb" "$out" || fail unpack "$name" "dpkg-deb -x succeeds" "dpkg-deb exit $?"
		echo "$sha" >"$out/.sha256"
		echo "fetch: $name: unpacked to $out"
	fi
done <"$VM_DIR/SOURCES"

echo "fetch: ok"
