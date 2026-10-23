//! Black-box tests of the MCP server (`MCP-01…05`; `MCPA-01…04`; the Streamable HTTP transport, `MCPH-01…06` of, in `streamable_http`): the
//! server runs in-process over piped stdio (a pair of in-memory pipes in
//! place of stdin and stdout), driven by an `rmcp` client, against a
//! recording local stub of the API.

#![cfg(test)]

mod aliases;
mod calls;
mod resources;
mod streamable_http;
mod support;
mod tools;
