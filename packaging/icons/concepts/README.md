# Logo concepts (UX review 6.3)

`compare.png` shows the current mark and both concepts at 16, 24, 32, 48 and 256 px on a dark
and a light surface. The "tuned" rows use the hand-drawn pixel grids, the "downscaled" rows use the
256 px master reduced the way `generate.ps1` does today.

| Concept | Idea | Files |
|---|---|---|
| A | Thumbnail card: rounded card with a mountain and sun, and a thumb-notch bitten out of one corner | `concept-a-card*.svg` |
| B | Carpenter's square forming the corner of a thumbnail frame, with graduation ticks and a dot of "picture" inside | `concept-b-square*.svg` |

Per concept:

- `concept-<x>.svg` is the 256 px master (32-unit grid scaled by 8).
- `concept-<x>-16.svg`, `-24.svg`, `-32.svg` are pixel-grid drawings: every shape sits on whole pixels
  (`shape-rendering="crispEdges"`), so a render at the matching size is exactly what was drawn.
- The `-light` copy of each file adds the darker-amber outline (`#b9761c`) for light surfaces.

Colours: amber `#e8a33d`, ink `#121417`, outline `#b9761c`.

After the owner picks one: copy the master over `../thumbwright.svg`, extend `../generate.ps1` to
render the 16, 24 and 32 px frames from the matching pixel-grid SVG (20 px from the 24, 40 px from
the 48 master), then regenerate `.ico`, `.res`, the MSIX logos and `site/img/icon.*`. The website has one
`<symbol id="tw-logo">` in `site/index.html` (marked LOGO SLOT) to swap as well.
