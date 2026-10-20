#!/bin/sh
# Rebuild the README demo and the social card. See README.md here.
#   ./make.sh [fonts|clip|record|render|web|social|all]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(dirname "$HERE")
OUT="$HERE/out"
mkdir -p "$OUT"
# Where in the rendered video the social card's frame is taken from.
SOCIAL_AT=36.575

fonts() {
  f="$HERE/render/fonts/SymbolsNerdFontMono-Regular.ttf"
  [ -f "$f" ] && return
  mkdir -p "$HERE/render/fonts"
  curl -fsSL -o "$OUT/nf.zip" \
    https://github.com/ryanoasis/nerd-fonts/releases/latest/download/NerdFontsSymbolsOnly.zip
  unzip -o -q "$OUT/nf.zip" SymbolsNerdFontMono-Regular.ttf -d "$HERE/render/fonts"
}

clip() {
  # Crab Rave, 1:15 to 1:44: the part with the crabs.
  ffmpeg -v error -y -ss 75 -to 104 -i "$REPO/docs/crabrave.webm" -an \
    -vf scale=640:-2 -c:v libx264 -crf 20 "$OUT/crabs.mp4"
}

record() {
  (cd "$REPO" && cargo build --release)
  python3 "$HERE/record.py"
}

render() {
  "$HERE/render/render" "$OUT/demo.cast" "$OUT/demo.mov" --start "$(cat "$OUT/start.txt")"
}

web() {
  # An animated AVIF: the whole video is ~12MB, where a GIF of it is 13MB
  # even at 640px and 10fps. A keyframe every second: with sparse ones,
  # browsers drop frames and slow down partway through.
  ffmpeg -v error -y -i "$OUT/demo.mov" -vf "fps=20,scale=1024:-2:flags=lanczos,format=yuv420p" \
    -c:v libsvtav1 -crf 38 -preset 6 -g 20 -loop 0 "$REPO/docs/demo.avif"
  # Shown where AVIF isn't supported.
  ffmpeg -v error -y -ss "$SOCIAL_AT" -i "$OUT/demo.mov" -frames:v 1 -vf scale=1280:-2 "$REPO/docs/demo-poster.png"
}

social() {
  # Paint the frame into the window on the existing card.
  ffmpeg -v error -y -ss "$SOCIAL_AT" -i "$OUT/demo.mov" -frames:v 1 "$OUT/social-frame.png"
  ffmpeg -v error -y -i "$REPO/docs/social-preview.png" -i "$OUT/social-frame.png" \
    -filter_complex "[1]scale=-1:574:flags=lanczos[f];[0][f]overlay=592:44" "$OUT/social.png"
  mv "$OUT/social.png" "$REPO/docs/social-preview.png"
}

case "${1:-all}" in
  all) fonts; clip; record; render; web; social ;;
  *) "$1" ;;
esac
