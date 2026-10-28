# YourTube icons

The icon is a rounded play button with a roof and a chimney, because your data stays at home. In the wordmark, "Your" is in the icon's coral and "Tube" is in ink.

Colours: coral `#FF6B4F` → `#EC4A36` (face gradient), chimney `#D63F2F`, base `#B5352B`, play triangle cream `#FFF7EE`, ink `#241F31` (light backgrounds) or `#FFFFFF` (dark backgrounds, `yourtube-wordmark-dark.svg`). The symbolic icon uses GNOME's `#2e3436` so GTK can recolour it. The wordmark type is Adwaita Sans Bold (SIL OFL), converted to outlines.

YourTube isn't affiliated with or endorsed by YouTube or Google.

`apple-touch-icon.png` (180×180) is the Home Screen icon for the web version on iPhones, which ignore SVG icons and fill transparency with black. Made from the app icon:

```sh
rsvg-convert -w 136 -h 136 io.github.yourtube.YourTube.svg -o house.png
magick -size 180x180 xc:"#FFF4EC" house.png -gravity center -geometry +0+2 -composite -strip PNG24:apple-touch-icon.png
```
