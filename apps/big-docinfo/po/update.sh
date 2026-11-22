#!/bin/bash
# Refresh big-docinfo.pot from the sources and merge it into every catalog.
set -euo pipefail
shopt -s nullglob
cd "${0%/*}/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
common=(--add-location=file --from-code=UTF-8 --add-comments=Translators)
xgettext "${common[@]}" --language=Rust --keyword=gettext --keyword=pgettext:1c,2 \
	-o "$tmp/rust.pot" src/main.rs
xgettext "${common[@]}" --language=Shell -o "$tmp/shell.pot" packaging/suggest-name
xgettext "${common[@]}" --language=Desktop -o "$tmp/desktop.pot" packaging/big-docinfo.desktop
msgcat --use-first --add-location=file "$tmp/rust.pot" "$tmp/shell.pot" "$tmp/desktop.pot" \
	-o po/big-docinfo.pot
for po in po/*.po; do
	msgmerge --quiet --update --backup=none --add-location=file "$po" po/big-docinfo.pot
done
