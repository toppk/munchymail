#!/usr/bin/env bash
# Build the documentation site with pandoc: site/build.sh [OUT] (default _site).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(dirname "$here")
out=${1:-"$repo_root/_site"}
theme="$here/theme"
edit=${MUNCHYMAIL_EDIT_URL-https://github.com/toppk/munchymail/edit/master}

# Reading order: page|title|group|source (source defaults to pages/<page>.md).
pages=(
  "index|Overview|Start"
  "tutorial|Tutorial|Start|docs/tutorial.md"
  "install|Install|Start|docs/install.md"
  "design|Design|How it works|docs/design.md"
  "ingest|Reading sources safely|How it works|docs/ingest.md"
  "internals|Index internals|How it works|docs/index.md"
  "sync|Keeping mail current|Coming next|docs/sync.md"
)

field() { cut -d'|' -f"$2" <<<"$1"; }
source_of() {
  local s
  s=$(field "$1" 4)
  if [[ -n "$s" ]]; then echo "$repo_root/$s"; else echo "$here/pages/$(field "$1" 1).md"; fi
}

nav_for() {
  local current=$1 group="" html="" p name title g
  for p in "${pages[@]}"; do
    name=$(field "$p" 1) title=$(field "$p" 2) g=$(field "$p" 3)
    if [[ "$g" != "$group" ]]; then
      [[ -n "$group" ]] && html+="</ul></div>"
      html+="<div class=\"nav-group\"><span class=\"nav-label\">$g</span><ul>"
      group=$g
    fi
    if [[ "$name" == "$current" ]]; then
      html+="<li><a href=\"$name.html\" aria-current=\"page\">$title</a></li>"
    else
      html+="<li><a href=\"$name.html\">$title</a></li>"
    fi
  done
  echo "$html</ul></div>"
}

rm -rf "$out"
mkdir -p "$out/theme" "$out/assets"
cp "$theme/horizon.css" "$theme/horizon.js" "$out/theme/"
cp -r "$here/assets/." "$out/assets/"
cp "$here/llms.txt" "$out/llms.txt"
: >"$out/llms-full.txt"
touch "$out/.nojekyll"

count=${#pages[@]}
for i in "${!pages[@]}"; do
  p=${pages[$i]}
  name=$(field "$p" 1)
  src=$(source_of "$p")
  pager=""
  if ((i > 0)); then
    prev=${pages[$((i - 1))]}
    pager+="<a class=\"prev\" href=\"$(field "$prev" 1).html\"><span>Previous</span>$(field "$prev" 2)</a>"
  fi
  if ((i < count - 1)); then
    next=${pages[$((i + 1))]}
    pager+="<a class=\"next\" href=\"$(field "$next" 1).html\"><span>Next</span>$(field "$next" 2)</a>"
  fi
  args=()
  [[ "$name" != "index" ]] && args+=(--toc --toc-depth=3)
  [[ -n "$edit" ]] && args+=(--metadata edit-url="$edit/${src#"$repo_root/"}")
  # Repo docs start with their own "# Title"; the template prints the title.
  body=$(if [[ "$src" == "$repo_root/docs/"* ]]; then sed '1{/^# /d}' "$src"; else cat "$src"; fi)
  pandoc \
    --from markdown+fenced_divs+bracketed_spans \
    --to html5 \
    --template "$theme/template.html" \
    --lua-filter "$theme/horizon.lua" \
    --metadata-file "$here/site.yaml" \
    --metadata title="$(field "$p" 2)" \
    --metadata source="$name.md" \
    --variable nav="$(nav_for "$name")" \
    --variable pager="$pager" \
    "${args[@]}" \
    --output "$out/$name.html" <<<"$body"
  cp "$src" "$out/$name.md"
  { printf '\n\n<!-- %s.md -->\n\n' "$name"; cat "$src"; } >>"$out/llms-full.txt"
done

echo "built $count pages into $out"
