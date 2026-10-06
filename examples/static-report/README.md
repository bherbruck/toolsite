# static-report

A report as one HTML file: no build step, no script, no handler. The
smallest thing toolsite publishes.

## What it shows

- A page in `public/index.html`, published as-is.
- Relative paths only, because every app is served from `/p/<slug>/`.
- Light and dark themes from `prefers-color-scheme`, and a print style.
- A chart drawn in inline SVG, so the page needs nothing from a CDN.

## Start it

```sh
toolsite init my-report --example static-report
cd my-report
toolsite deploy
```

Without the CLI: `create_upload`, then `curl -fT public/index.html <upload-url>`.

## Change it

Edit `public/index.html` and deploy again. Reach for a Vite project
(`toolsite init --react`) as soon as the page needs state or a second
screen.
