# E133 import_drop

The canvas's blueprint drop (#269). A folder holding `blueprint.yaml`, dragged from the
file tree onto the canvas, is not opened as a tree: it is previewed, and imported only
when the human approves.

1. the tree lists the folder `bp`; dropping it on the canvas opens the `import blueprint` dialog with `would create 3 agents`; nothing is created yet;
2. Cancel: still nothing, and no further call to `user`;
3. dropped again and approved: the agents exist, under the canvas host, and a toast says `imported <ids>`;
4. dropped on a cell: the agents go under that cell's backend;
5. a blueprint with a port: the `import: bind ports` dialog asks for the agent; typed and previewed, then approved, it imports and binds;
6. a blueprint carrying code is refused with a toast naming `allow_code`; nothing is created;
7. no page error.

Scripted (no model, no key); drives headless Chrome over CDP. Without Chrome on the machine
(`$CHROME` or the stock macOS path) it prints `E2E-SKIP: …`.
