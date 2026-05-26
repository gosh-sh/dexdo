# openapi/

OpenAPI 3.1 contract for the Dodex REST API and the static page that renders it.

## Files

- `openapi.yaml` — the spec. **Generated from Rust**; do not edit by hand.
- `index.html` — [Scalar](https://scalar.com) reference page, loads `openapi.yaml` from the same folder.
- `generate.sh` — regenerates `openapi.yaml` from `services/api` and validates it with `@redocly/cli`.

## Regenerate after changing handlers or DTOs

```sh
openapi/generate.sh
```

Equivalent without the wrapper:

```sh
cargo run -p dodex-api --bin gen-openapi -- --out openapi/openapi.yaml
npx -y @redocly/cli@latest lint openapi/openapi.yaml
```

Commit the updated `openapi.yaml` together with the handler or DTO change.

## Preview locally

```sh
python3 -m http.server -d openapi 8080
```

Then open <http://localhost:8080/>. Any static file server pointed at this folder works.

## Deployment

`.github/workflows/pages.yml` deploys this folder to GitHub Pages on every push to `dev` that touches `openapi/**`. The live URL appears in the workflow's Deploy step output.

One-time setup in the repo: **Settings → Pages → Build and deployment → Source = GitHub Actions**.
