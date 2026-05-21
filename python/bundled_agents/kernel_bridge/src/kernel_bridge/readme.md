# kernel_bridge — cross-kernel comms
Pairs of bridge agents forward `send` envelopes between kernels over memory / WS / SSH+WS / HTTP transports. Weak binding — the remote is addressed by URL + path only, no shared types.
