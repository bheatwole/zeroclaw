`manifest.json` references `color.png` and `outline.png`, required by the
Teams app package format:

- `color.png` -- 192x192 px, full color, no transparency.
- `outline.png` -- 32x32 px, white-on-transparent, simple silhouette.

No placeholder image files are committed here -- a fake/invalid PNG would
fail Teams' app validation just as loudly as a missing one, so it's not
worth pretending. Generate real icons before packaging the app (zip
`manifest.json` + both PNGs together) and drop them in this directory under
these exact names.
