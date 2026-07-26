# hrdr-ui

Dioxus WASM single-page application for the hrdr web server.

## Build

```bash
# Install the Dioxus CLI (once).
cargo install --locked dioxus-cli@0.7

# Build the WASM app.
cd crates/hrdr-ui
dx build --platform web --release

# Copy the output to the dist/ folder so the server can embed it.
# The exact output path is printed at the end of the dx build step.
# Typical path under target/dx/hrdr-ui/... — read it from the build output.
cp -r <printed-path> dist/

# Then build hrdr with the UI feature.
cd ../..
cargo run --features hrdr-web/ui -- serve
```

Open `http://127.0.0.1:9911/?token=<token>` in a browser.
