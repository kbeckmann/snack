#!/usr/bin/env bash
# Build the transparent-background variant: just the chat bubble + mesh, no
# gradient squircle. qlmanage renders the SVG on white (it can't keep alpha), so
# we clip the render to the bubble silhouette — drawn here from the same path as
# snack-transparent.svg — which punches everything outside the bubble transparent.
set -euo pipefail
cd "$(dirname "$0")"

BUBBLE="M 218,63 L 806,63 A 155,155 0 0 1 961,218 L 961,635 A 155,155 0 0 1 806,790 L 455,790 C 392,860 281,917 185,925 C 270,884 281,834 314,790 L 218,790 A 155,155 0 0 1 63,635 L 63,218 A 155,155 0 0 1 218,63 Z"

qlmanage -t -s 1024 -o . snack-transparent.svg >/dev/null
mv -f snack-transparent.svg.png _r.png
magick -size 1024x1024 xc:none -fill white -draw "path '$BUBBLE'" _m.png
magick _r.png _m.png -alpha set -compose DstIn -composite _bubble.png
rm -f _r.png _m.png

# The bubble floats free (no squircle), so give it a generous margin inside the
# tile — a shape that touches the tile edges looks oversized next to framed icons.
MARGIN=80
INNER=$((1024 - 2 * MARGIN))          # 864 — same bounding box as native squircle icons
magick -size 1024x1024 xc:none \( _bubble.png -resize ${INNER}x${INNER} \) -gravity center -composite snack-transparent-1024.png
rm -f _bubble.png

rm -rf _t.iconset
mkdir _t.iconset
for pair in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x 256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
  px=${pair%%:*}; name=${pair##*:}
  sips -s format png -z "$px" "$px" snack-transparent-1024.png --out "_t.iconset/icon_${name}.png" >/dev/null 2>&1
done
iconutil -c icns _t.iconset -o snack-transparent.icns
rm -rf _t.iconset
sips -s format png -z 256 256 snack-transparent-1024.png --out snack-transparent-256.png >/dev/null 2>&1
echo "Built snack-transparent-1024.png, snack-transparent.icns, snack-transparent-256.png"
