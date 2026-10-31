#!/usr/bin/env sh
# Makes the icon set in web/assets/icons/ (see the README there) from a square master PNG, plus
# optionally a 1200 x 630 card for link previews and a simplified master for the 16 and 32 px favicons.
# Needs ImageMagick (7's `magick`, or 6's `convert`).
#
#   tools/icons.sh [master.png [og.png [small.png]]]
#
# With no arguments it uses the committed tools/icons/radar-master.png, radar-og.png and
# radar-master-small.png (see tools/icons/polish.py). Given a master alone, the card is the master on
# navy and the favicons come from the master too.
set -eu

dir=$(dirname "$0")
[ $# -le 3 ] || { echo "usage: $0 [master.png [og.png [small.png]]]" >&2; exit 2; }
if [ $# -eq 0 ]; then
  set -- "$dir/icons/radar-master.png" "$dir/icons/radar-og.png" "$dir/icons/radar-master-small.png"
fi
master=$1 og=${2:-} small=${3:-$1}
for f in "$master" $og "$small"; do [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }; done
out=$dir/../web/assets/icons
# The site's dark page color, behind the preview fallback; and the tile's navy, behind the iOS icon
# (which can't be transparent), so its corners match the tile.
bg='#0a1321'
tile='#001a3f'

if command -v magick >/dev/null 2>&1; then im=magick; id="magick identify"; else im=convert; id=identify; fi
for f in "$master" "$small"; do
  set -- $($id -format '%w %h' "$f[0]")
  [ "$1" = "$2" ] || { echo "$f is $1 x $2: a master must be square" >&2; exit 1; }
done
set -- $($id -format '%w' "$master[0]")
[ "$1" -ge 512 ] || echo "warning: $master is only $1 px; 1024 or more looks best at 512" >&2

mkdir -p "$out"
png() { $im "$1" -resize "$2x$2" -strip "PNG32:$out/$3"; }
png "$small" 16 favicon-16.png
png "$small" 32 favicon-32.png
png "$small" 96 mark-96.png  # the header's logo: 22 to 26 px, on screens up to 3x
png "$master" 192 icon-192.png
png "$master" 512 icon-512.png
$im "$master" -resize 180x180 -background "$tile" -alpha remove -alpha off -strip "PNG24:$out/apple-touch-icon.png"
$im "$small" -define icon:auto-resize=48,32,16 "$out/favicon.ico"
if [ -n "$og" ]; then
  $im "$og" -resize 1200x630^ -gravity center -extent 1200x630 -strip "PNG24:$out/og.png"
else
  $im -size 1200x630 "xc:$bg" \( "$master" -resize 400x400 \) -gravity center -composite -strip "PNG24:$out/og.png"
fi
ls -l "$out"
