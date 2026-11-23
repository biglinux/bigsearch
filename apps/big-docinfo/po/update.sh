#!/bin/bash
# Refresh big-docinfo.pot from the sources and merge it into every catalog.
set -euo pipefail
shopt -s nullglob
cd "${0%/*}/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
common=(--add-location=file --from-code=UTF-8 --add-comments=Translators
	--package-name=big-docinfo --msgid-bugs-address=https://github.com/biglinux/bigsearch/issues
	--copyright-holder="BigLinux contributors")
xgettext "${common[@]}" --language=Rust --keyword=gettext --keyword=pgettext:1c,2 \
	-o "$tmp/rust.pot" src/main.rs
xgettext "${common[@]}" --language=Shell -o "$tmp/shell.pot" packaging/suggest-name
xgettext "${common[@]}" --language=Desktop -o "$tmp/desktop.pot" packaging/big-docinfo.desktop
msgcat --use-first --add-location=file "$tmp/rust.pot" "$tmp/shell.pot" "$tmp/desktop.pot" \
	-o "$tmp/big-docinfo.pot"
# The first comment lines are xgettext's template; name the package instead.
sed '1,4c\# Messages of big-docinfo.\n# This file is distributed under the MIT license, like big-docinfo.' \
	"$tmp/big-docinfo.pot" >po/big-docinfo.pot
for po in po/*.po; do
	msgmerge --quiet --update --backup=none --add-location=file "$po" po/big-docinfo.pot
done
