# blob-gallery

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- An upload is two steps. `POST /api/photos` makes a row that is not ready
  and two upload URLs. `POST /api/photos/<id>/ready` checks both files with
  `blobs.stat` and lists the photo. A failed upload leaves a row that never
  shows.
- A file that is not an image is deleted at the ready step.
- The browser draws the thumbnail. A handler has no image decoder, and
  decoding untrusted images on the server is a risk in any case.

## Unfinished

- Rows that never became ready are not cleaned up. A `[[job]]` could do it.
