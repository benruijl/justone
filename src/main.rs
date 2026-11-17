//! Self-hosted Just One server: a single binary that serves the page and runs the games.
//!
//! It speaks the same WebSocket protocol as the Cloudflare Worker (see `room.rs`). Games are
//! kept in memory, so they are lost when the server restarts.
//!
//! Configuration through environment variables: `PORT` (default 3030) and `HOST`
//! (default 0.0.0.0). For HTTPS, put it behind a reverse proxy such as Caddy or nginx.

#[cfg(not(target_arch = "wasm32"))]
mod native_server;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native_server::main();
}

// the wasm32 build is the Cloudflare Worker in the library
#[cfg(target_arch = "wasm32")]
fn main() {}
