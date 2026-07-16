#!/usr/bin/env sh
# Waits for a just-published crate version to show up on the crates.io sparse
# index — the same source `cargo` itself resolves dependencies against. Used
# between sequential `cargo publish` calls in the release workflow (issue #362)
# so a dependent crate (e.g. entanglement-core depending on
# entanglement-provider) doesn't fail resolution on a not-yet-propagated
# registry dep.
#
# Usage: wait-for-crate.sh <crate-name> <version> [timeout-seconds]
set -eu

if [ "$#" -lt 2 ]; then
	echo "usage: wait-for-crate.sh <crate-name> <version> [timeout-seconds]" >&2
	exit 2
fi

name=$1
version=$2
timeout=${3:-300}

# crates.io sparse index path convention (see cargo book, "Registry Index"):
# 1 char -> 1/{name}; 2 chars -> 2/{name}; 3 chars -> 3/{first}/{name};
# 4+ chars -> {first-two}/{next-two}/{name}. All our crate names are 4+ chars.
len=${#name}
if [ "$len" -ge 4 ]; then
	p1=$(printf '%s' "$name" | cut -c1-2)
	p2=$(printf '%s' "$name" | cut -c3-4)
	path="$p1/$p2/$name"
elif [ "$len" -eq 3 ]; then
	p1=$(printf '%s' "$name" | cut -c1-1)
	path="3/$p1/$name"
else
	path="$len/$name"
fi

url="https://index.crates.io/$path"
elapsed=0
interval=5

echo "waiting for $name $version to appear on the sparse index ($url)..."
while [ "$elapsed" -lt "$timeout" ]; do
	if curl -fsS "$url" 2>/dev/null | grep -q "\"vers\":\"$version\""; then
		echo "$name $version is indexed"
		exit 0
	fi
	sleep "$interval"
	elapsed=$((elapsed + interval))
done

echo "FAIL: timed out after ${timeout}s waiting for $name $version on the index" >&2
exit 1
