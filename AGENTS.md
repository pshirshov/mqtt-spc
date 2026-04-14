# SPC-MQTT Bridge

Vanderbilt SPC4300 alarm panel to MQTT bridge for Home Assistant integration.

## Build

- `cargo build --release` for development
- `nix build` for reproducible builds

## Nix cargoHash

After changing `Cargo.toml` dependencies, you **must** update `cargoHash` in `flake.nix`:

1. Set `cargoHash = "";` temporarily
2. Run `nix build` — it will fail and print `got: sha256-...`
3. Replace the empty string with the printed hash
4. Run `nix build` again to verify

## Credentials

Never read credential files (`creds.json`, `mqtt-creds.json`) directly. Only write code that reads them at runtime.
