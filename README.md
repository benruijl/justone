Just One
========

A web version of the board game [Just One](https://www.boardgamegeek.com/boardgame/254640/just-one). If you like this (unaffiliated) digital version, support the original designers by buying their game.

If you'd like to [support](https://www.buymeacoffee.com/benruijl) me, you can buy me a coffee!

Playing
-------
To play the game online, you can host a server yourself or go to https://justone.duckdns.org/.

Screenshots
-----------
<img src="include/justone_gameplay.png" alt="Gameplay" width="400"/>
<br/>
<img src="include/justone_gameplay2.png" alt="Gameplay" width="400"/>


Hosting
-------
The game runs as a [Cloudflare Worker](https://developers.cloudflare.com/workers/) and fits in the free plan.
The page in `public/` is served as static assets, and each game is a Durable Object that holds the
players' WebSockets and stores the game state. Games are removed when they finish or after 90 minutes
without activity.

Requirements: Rust with the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`) and Node.js.

    npx wrangler dev      # run locally on http://localhost:8787
    npx wrangler deploy   # deploy to your Cloudflare account

Development
-----------
The game logic is in `src/game.rs` (test it with `cargo test`), and the Worker and Durable Object are in `src/worker_app.rs`.
Word lists are in `include/words_*.txt` and are compiled into the Worker.

Any suggestions, word lists, and patches are welcome!