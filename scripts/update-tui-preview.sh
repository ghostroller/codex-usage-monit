#!/bin/sh

set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
generated_preview="$repository_root/target/tui-gallery/overview-dark-120x40.svg"
published_preview="$repository_root/docs/assets/tui/overview-dark-120x40.svg"
mode=${1:-update}

case "$mode" in
  update|--check)
    ;;
  *)
    echo "usage: sh scripts/update-tui-preview.sh [update|--check]" >&2
    exit 64
    ;;
esac

cd "$repository_root"
cargo test --locked --lib \
  tui::tests::integration_scenarios::svg_gallery_is_generated_from_the_same_semantic_frames \
  -- --exact

if [ "$mode" = "--check" ]; then
  if [ ! -f "$published_preview" ] || ! cmp -s "$generated_preview" "$published_preview"; then
    echo "published TUI preview is stale" >&2
    echo "run: sh scripts/update-tui-preview.sh" >&2
    exit 1
  fi
  echo "published TUI preview is up to date"
  exit 0
fi

mkdir -p "$(dirname -- "$published_preview")"
cp "$generated_preview" "$published_preview"
echo "updated docs/assets/tui/overview-dark-120x40.svg"
