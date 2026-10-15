# filmcraft-media

Media sources, still-image and image-sequence import, generated media, audio streams, and decoded
frame caching for FilmCraft.

## Stills

`STILL_EXTENSIONS` lists what the import dialogs and the Media Browser offer: PNG, JPEG, GIF, WebP,
TIFF, BMP, Targa, DPX, OpenEXR, Radiance HDR and DNG are decoded here; HEIF / HEIC (`.heic`,
`.heif`, `.hif`) and AVIF (`.avif`) by `filmcraft-codecs` (`heif`), which hands the picture to
`StillSource::from_rgba` (`with_codec` names it "HEIC" or "AVIF"). Image sequences of those take
the codecs' still decoders through `ImageSequenceSource::with_decoders` (`still::StillDecoder`).

## Image-sequence filenames

`sequence::Numbered` identifies the last run of 1–18 ASCII frame-number digits in a filename's
stem when the extension is a recognized still-image format. `/` and `\` separate directory
components. A prefix may contain other UTF-8 characters, including accented letters, CJK and emoji:
`é0001.png`, `图0001.png` and `🎞️0001.png`. Parsing preserves the prefix, number padding and suffix
when generating sibling-frame paths. Sibling recognition compares the prefix's UTF-8 bytes exactly.
Stems without ASCII digits or with a final frame-number run longer than 18 characters are not numbered
image-sequence filenames.
