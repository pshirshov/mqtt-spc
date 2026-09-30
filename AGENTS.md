# SPC-MQTT Bridge

Vanderbilt SPC4300 alarm panel to MQTT bridge for Home Assistant integration.
The panel connects to this bridge as an EDP v2 receiver (protocol per https://github.com/imduffy15/spcedp).

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

Never read credential files (`mqtt-creds.json`, the EDP key file passed via `--edp-key-file`) directly. Only write code that reads them at runtime.
