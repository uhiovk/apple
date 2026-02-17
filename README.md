# My Audio Transcoding Pipeline

"APPLE" maybe comes from "**A**utomated **P**i**p**e**l**ined Audio **E**ncoder", IDRK.

I built this for my own specific use case, that is:
1. Decrypt .ncm files downloaded from NCM into inner format (i.e. FLAC, MP3)
2. Convert that format into OGG encapsulated Opus
3. Resize and transcode cover images to reduce file size
4. Reserve basic metadata fields
5. Insert externally downloaded lyrics and filter them with custom logic (todo)
6. Disk space saved

Why do I do this:
- `FFmpeg` and `opus-tools` aren't flexible enough
  - It's pure torture building them on Windows
  - I don't like prebuilt ones (from MSYS2, I mean)
- I don't like to introduce a lot of intermediate files
  - SSDs are expensive
- I never remember how to use `parallel`
- My old workflow is basically too slow and not enoughly automated
  - Apparently this new one is at least three times slower than `opusenc` and I have no clue
