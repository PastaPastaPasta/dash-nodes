#!/usr/bin/env sh
# Makes the icon set in web/assets/icons/ (see the README there) from one square master PNG, and
# optionally a 1200 x 630 master for link previews. Needs ImageMagick (7's `magick`, or 6's `convert`).
#
#   tools/icons.sh master.png [og-master.png]
set -eu

[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 master.png [og-master.png]" >&2; exit 2; }
master=$1
og=${2:-}
out=$(dirname "$0")/../web/assets/icons
# The site's dark page color, behind the iOS icon (which can't be transparent) and the preview.
bg='#0a1321'

if command -v magick >/dev/null 2>&1; then im=magick; else im=convert; fi
size=$($im identify -format '%w %h' "$master" 2>/dev/null || identify -format '%w %h' "$master")
set -- $size
[ "$1" = "$2" ] || { echo "$master is $1 x $2: the master must be square" >&2; exit 1; }
[ "$1" -ge 512 ] || echo "warning: $master is only $1 px; 1024 or more looks best at 512" >&2

mkdir -p "$out"
png() { $im "$master" -resize "$1x$1" -strip "PNG32:$out/$2"; }
png 16 favicon-16.png
png 32 favicon-32.png
png 192 icon-192.png
png 512 icon-512.png
$im "$master" -resize 180x180 -background "$bg" -alpha remove -alpha off -strip "PNG24:$out/apple-touch-icon.png"
$im "$master" -define icon:auto-resize=48,32,16 "$out/favicon.ico"
if [ -n "$og" ]; then
  $im "$og" -resize 1200x630^ -gravity center -extent 1200x630 -strip "PNG24:$out/og.png"
else
  $im -size 1200x630 "xc:$bg" \( "$master" -resize 400x400 \) -gravity center -composite -strip "PNG24:$out/og.png"
fi
ls -l "$out"
