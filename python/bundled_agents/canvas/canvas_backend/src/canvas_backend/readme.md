# canvas_backend — spatial host
Members are structural children: `add_agent handler_module=X` spawns one, `list_members` lists them, `remove_agent` cascades one out, `discover` returns members intersecting a spatial rect. Probes each member for get_webapp (DOM iframe) and get_gl_view (WebGL layer).
