#!/usr/bin/env bash
# Regenerate transparent, macOS-margined icon assets from icon/snack.svg.
# qlmanage renders the SVG (gradients + filters) well but composites on white,
# and ImageMagick can't render this SVG — so: qlmanage renders the art at the
# inset size, then ImageMagick centers it on a transparent 1024 canvas and clips
# it to the squircle (punching the margin + rounded corners transparent).
set -euo pipefail
cd "$(dirname "$0")"

# Match the size of native macOS icons. Measured opaque coverage of Messages /
# Finder / Terminal is ~0.68-0.70 of the tile — they are NOT full-bleed; they
# carry an ~80px transparent margin (an ~862px squircle in a 1024 canvas). A
# full-bleed icon renders visibly larger than every neighbour (and overflows in
# the cmd-tab switcher); a 100px margin renders slightly smaller than native.
MARGIN=80
INNER=$((1024 - 2 * MARGIN))          # 864
FAR=$((1024 - MARGIN))                 # 944
RADIUS=194                             # 230 * 864/1024, matching the SVG squircle

qlmanage -t -s "$INNER" -o . snack.svg >/dev/null
mv -f snack.svg.png _render.png
magick -size 1024x1024 xc:none _render.png -geometry +${MARGIN}+${MARGIN} -composite _placed.png
magick -size 1024x1024 xc:none -fill white -draw "roundrectangle ${MARGIN},${MARGIN},${FAR},${FAR},${RADIUS},${RADIUS}" _mask.png
magick _placed.png _mask.png -alpha set -compose DstIn -composite snack-1024.png
rm -f _render.png _placed.png _mask.png

rm -rf snack.iconset
mkdir snack.iconset
for pair in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x 256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
  px=${pair%%:*}; name=${pair##*:}
  sips -s format png -z "$px" "$px" snack-1024.png --out "snack.iconset/icon_${name}.png" >/dev/null 2>&1
done
iconutil -c icns snack.iconset -o snack.icns
rm -rf snack.iconset
sips -s format png -z 256 256 snack-1024.png --out snack-256.png >/dev/null 2>&1
echo "Regenerated transparent + margined snack-1024.png, snack.icns, snack-256.png"
