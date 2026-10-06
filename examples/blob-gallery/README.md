# blob-gallery

A photo gallery on the app's file store. Photos go from the browser
straight to storage, and come back the same way.

## What it shows

- `blobs.upload-url`: the handler hands the browser a one-time URL and the
  browser PUTs the file there. The handler never holds the bytes.
- Thumbnails drawn in the browser with a canvas, uploaded as a second file.
- `blobs.stat` to check that both files arrived and are images before a
  photo is listed.
- Serving with `x-toolsite-blob: <key>` and an empty body, at any size.
- `blobs.delete`, allowed for the person who added the photo or a
  `curator`.

## Start it

```sh
toolsite init my-gallery --example blob-gallery
cd my-gallery
toolsite deploy
```

Files go to the site's volume, or to its bucket when one is set up.

## Files

- `handler/src/lib.rs`: begin, ready, serve, remove.
- `src/App.tsx`: the grid, the upload button and the thumbnail drawing.
- `migrations/001_initial.sql`: one row per photo.
