# openapi/

Generator for the OpenAPI 3.1 contract. The published artefacts
(`openapi.yaml` and the Scalar viewer `index.html`) live in
[`../docs/`](../docs/) because GitHub Pages deploys from that folder.

## Files

- `generate.sh` — regenerates `docs/openapi.yaml` from `services/api` and validates it with `@redocly/cli`.

The spec itself is `docs/openapi.yaml` (**generated from Rust**; do not edit by hand) and the renderer is `docs/index.html`.

## Regenerate after changing handlers or DTOs

```sh
openapi/generate.sh
```

Equivalent without the wrapper:

```sh
cargo run -p dodex-api --bin gen-openapi -- --out docs/openapi.yaml
npx -y @redocly/cli@latest lint docs/openapi.yaml
```

Commit the updated `docs/openapi.yaml` together with the handler or DTO change. CI re-runs the generator and fails if the committed spec drifted from the Rust source — see `.github/workflows/openapi.yml`.

## Preview locally

```sh
python3 -m http.server -d docs 8080
```

Then open <http://localhost:8080/>. Any static file server pointed at `docs/` works.

## Deployment

`.github/workflows/pages.yml` deploys `docs/` to GitHub Pages on every push to `dev` that touches `docs/**`. The live URL appears in the workflow's Deploy step output.

One-time setup in the repo: **Settings → Pages → Build and deployment → Source = GitHub Actions**.
