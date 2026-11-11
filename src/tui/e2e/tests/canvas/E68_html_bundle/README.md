# E68 html_bundle

The html side of E67 (owner 2026-10-09: "all same with html agent — creation, edit, bundling"), driven the way a coding agent does it:

a) double-click → `html` places an html cell (its starter body);
b) `set_html` sent to `<guid>|<cell>` through the host re-renders it (`HTML-E68-A`);
c) the body (`get_html` gives back what (b) set) as `.fantastic/web/bundles/card_e68/bundle.yaml` (`base: html_agent`, `html: |`), on disk before the page opens (a page loads its folder bundles when it opens), is a tile in the popup's `page · bundles` group;
d) that tile places an instance showing `HTML-E68-A`;
e) the edited cell of (b) keeps its own body.

Run: `python3 e2e/run.py canvas/E68_html_bundle` (or the probe directly).
