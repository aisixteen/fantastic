import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { credentialRows, credentialsState, grantsOf } from "../agent_models/credentials_model.ts";

// credential_store: ref NAMES, hosts, field NAMES and who may use the store.
// NEVER a value: it reads `list` (metadata only) and `reflect` (the grants), and
// the grant has no `get`.
export const CREDENTIALS_VIEW = "credentials_view.ts";
export const CREDENTIALS_CALLS = ["list", "reflect", "watch"] as const;

export const credentialsView: ViewBundle = {
  handles: [CREDENTIALS_VIEW],
  grants: (backend) => ({ [backend]: [...CREDENTIALS_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    return mountReadView<{ list: unknown; reflect: unknown }>(ctx, {
      name: "credentials",
      async load(call) {
        const [list, reflect] = await Promise.all([call({ type: "list" }), call({ type: "reflect" })]);
        return { list, reflect };
      },
      shape: (d) => credentialRows(credentialsState(d.list), grantsOf(d.reflect)),
    });
  },
};
