# canvas_backend — spatial UI host as an agent
Membership is structural: members are direct children, cascade-delete owns the subtree. Verbs: `add_agent`, `remove_agent`, `list_members`, `discover` (spatial intersect). Members must answer `get_webapp` (HTML iframe) or `get_gl_view` (GL scene); add_agent refuses if neither.
