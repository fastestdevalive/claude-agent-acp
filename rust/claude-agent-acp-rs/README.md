# claude-agent-acp-rs

## Experimental ACP v2

Started with `CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1`, the adapter negotiates the
[draft ACP v2](https://agentclientprotocol.com/protocol/v2/overview): a client that asks for
protocol version 2 or later in `initialize` gets v2, and every other client gets v1, as before.
Without the opt-in the adapter serves ACP v1 alone and never routes by protocol version. The
draft can still change, and this surface changes with it.

In this port the opt-in currently covers the `initialize` handshake: after a v2 negotiation,
every later request is answered with the JSON-RPC method-not-found error (`-32601`) rather than
in a v1 shape, until the v2 agent surface is wired. Steering and the AIR extensions are v1 only
for now.
