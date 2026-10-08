# qrtx

Connect two computers by scanning a QR code on each with your phone.

Each computer shows a QR code. Your phone scans both and introduces them, then
they connect directly to each other over [iroh](https://iroh.computer):
end-to-end encrypted, with NAT hole punching and a relay as fallback. Your data
never goes through the phone or the website. Inspired by
[dumbpipe](https://github.com/n0-computer/dumbpipe).

Works on Linux and macOS, x86-64 and arm64.

## Using qrtx.lol

`curl https://qrtx.lol` prints a short version of this section.

**SSH into a machine** (one session, no open ports):

```sh
curl -fsSL https://qrtx.lol/sshd | sh                      # on the server
curl -fsSL https://qrtx.lol/ssh | sh -s -- me@myserver     # on your laptop
```

**Install** and use it for anything else:

```sh
curl -fsSL https://qrtx.lol/install | sh

qrtx < backup.tar                              # machine A: send a file
qrtx > backup.tar                              # machine B: receive it

qrtx listen-tcp --host localhost:5432          # machine A: share a TCP port
qrtx connect-tcp --addr 127.0.0.1:5432         # machine B: use it locally
```

**Then pair them:** scan one QR code with your phone's camera app. That opens
qrtx.lol. Scan the other code from that page. Done.

Notes:

- To run once without installing, use `curl -fsSL https://qrtx.lol/run | sh -s -- <args>`.
  That leaves no stdin for your data, so to pipe data that way use
  `sh -c "$(curl -fsSL https://qrtx.lol/run)" qrtx < file`.
- `qrtx sshd` lets in one connection, then exits.
- `qrtx ssh` runs your normal `ssh` with qrtx as its proxy, so ssh options and
  host key checks work as usual. `myserver` is only the name the host key is
  saved under.
- `qrtx --help` lists everything.

## Self-hosting

The site is plain static files, with no server-side code. To host your own:

```sh
# needs Rust 1.91+, plus:
rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install wasm-bindgen-cli --locked --version 0.2.129   # must match Cargo.lock
cargo install cargo-zigbuild && pip install ziglang          # to cross-compile Linux binaries

export QRTX_SITE=https://qrtx.example.com
./build.sh            # builds everything into ./site
```

`QRTX_SITE` goes into the install scripts (where to download from) and into
the binaries (where the QR codes point).

macOS binaries have to be built on a Mac. With the same `QRTX_SITE`, run:

```sh
./build.sh bin aarch64-apple-darwin x86_64-apple-darwin
```

Then copy the resulting `site/dl/qrtx-darwin-*.gz` into your `site/dl/` and run
`./build.sh checksums`.

Upload the `site/` directory to any static host that serves HTTPS (the phone
camera only works on HTTPS pages). Users then run
`curl -fsSL https://qrtx.example.com/install | sh`.

Other commands:

```sh
./build.sh serve     # try it locally on http://localhost:8000
./build.sh test      # end-to-end test (needs chromium and node)
```

`.github/workflows/site.yml` builds all platforms, tests them, and deploys to
GitHub Pages.

## How it works

1. Each machine starts an iroh endpoint. Its QR code holds the endpoint's public
   key, its relay, and a one-time secret. These sit in the URL `#fragment`, so
   they never reach the web server.
2. The web page runs iroh in the browser (compiled to wasm). It dials each
   machine, checks the key from the code, and proves it saw the code by sending
   the secret.
3. It tells each machine about the other. They connect directly. Each machine
   only accepts the peer it was introduced to, and only one pairing per run.

The code is in `crates/proto` (QR format and protocol), `crates/cli` (the
`qrtx` binary), `crates/web` (the browser side), and `site/` (the page).
`scripts/qrtx.sh` is the template for `/run`, `/install`, `/ssh` and `/sshd`.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
