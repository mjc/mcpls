# HTTP trust boundary

MCPLS exposes edit-capable MCP tools. The built-in Streamable HTTP transport
therefore binds to loopback only. Host allow-listing protects against DNS
rebinding, but it does not authenticate callers or authorize edits.

Remote access must use an authenticated, TLS-terminating reverse proxy. The
proxy should:

- authenticate the caller before forwarding MCP requests;
- preserve the configured MCP path and use a loopback upstream;
- restrict access to the intended clients and networks;
- avoid forwarding untrusted Host values to MCPLS.

The MCP routing headers (`Mcp-Method` and `Mcp-Name`) help an authenticating
gateway route and audit requests, but they are not authentication credentials.
MCPLS does not treat their presence as proof of identity.

MCPLS rejects direct non-loopback binds instead of treating an exposed socket
as trusted. Health and status responses contain lifecycle counts and version
metadata, not command arguments, environment values, or credentials.
