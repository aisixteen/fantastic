# gl_agent — GL-view-as-record
`gl_source` JS body, compiled by a canvas host into its own per-view `THREE.Group` container (the GL analogue of an html_agent iframe). `set_gl_source` edits it live — emits `gl_source_changed`, the canvas reinstalls that one view in place (dispose group + recompile), same agent id, no canvas refresh.
