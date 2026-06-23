# DEX.DO Shellnet Testing Guide

To start testing DEX.DO in Shellnet, prepare your own test Private Notes,
choose a backend to register them, and use the issued API keys for trading
scenarios.

## 1. Prepare Private Notes

Testing requires pre-deployed and funded Private Notes (PNs). Each trading
account works through a separate PN.

Deploy a PN pool using the `mint_pn_pool` tool:
<https://github.com/gosh-sh/dexdo/blob/dev/docs/seed-private-notes.md#producing-the-notes>

Make sure each PN is funded with SHELL tokens. SHELL is used as gas for trading
operations; a PN without SHELL cannot submit orders.

Instructions for getting test SHELL and using the Shellnet giver:
<https://dev.ackinacki.com/readme/get-test-tokens-in-shellnet#get-shell>

The `pn_pool.json` and `pn_pool.seed_notes.json` files contain PN access data,
including private keys. Do not commit them, do not publish them in logs, and
share them only as secrets.

For agent-driven Shellnet onboarding, see the root
[`README.md`](../README.md#onboarding-shellnet).

## 2. Choose a backend for PN registration

DEX.DO testing requires a backend where your PNs will be registered. You can either use the hosted Shellnet backend provided by the DEX.DO team or deploy your own BM and DEX.DO backend instances.

### Option 1: Use the hosted DEX.DO backend

Use the DEX.DO backend endpoint provided by the DEX.DO team:
<https://dodex-dev.ackinacki.org>.
(DEX.DO will not offer this hosted-backend option on Mainnet)

🚨 **Important:**
Loading a PN into any API service **delegates **the PrivateNote's
private key to the backend and does not provide any security guarantees for
delegated PN keys**

### Option 2: Deploy your own BM and DEX.DO backend

Deploy your own Block Manager (BM) instance using the Acki Nacki documentation:
<https://github.com/ackinacki/ackinacki/blob/main/README.md#deployment-overview>

During BM deployment, use the test BM license and BK endpoint provided to you.

Deploy a separate DEX.DO backend instance using this guide:

<https://github.com/gosh-sh/dexdo/blob/dev/docs/deployment.md>

Configure the backend to connect to your BM service and assigned BK endpoint.

For self-service registration through `POST /api/v1/accounts` on your backend,
keep `auth.seed_accounts` disabled (`false` or unset).

## 3. Register Trading Accounts

Use the backend selected in the previous step.

For each PN, create one API account:

```http
POST /api/v1/accounts
```

Request body:

```json
{
  "pnAddress": "<PN_address>",
  "pnPubkeyHex": "<PN_pubkey_hex>",
  "pnSeckeyHex": "<PN_seckey_hex>",
  "pnDihHex": "<PN_dih_hex>"
}
```

These fields can be taken from `pn_pool.seed_notes.json`. That file uses
snake_case field names (`pn_address`, `pn_pubkey_hex`, `pn_seckey_hex`, and
`pn_dih_hex`); the public API request uses camelCase field names.

Endpoint documentation:
<https://gosh-sh.github.io/dexdo/#tag/account/POST/api/v1/accounts>

The backend checks that the PN exists on-chain and that the submitted private
key matches the PN owner.

The backend response returns `apiKey` and `apiSecret`.

Store `apiKey` and `apiSecret` as secrets. The `apiSecret` is shown only during
registration and is required for signed private and trading requests.

Each PN can be registered once. If the backend returns a duplicate-registration
error, use another PN or the credentials issued during the first registration.

Full endpoint contract:
[`api-spec.md#register-account`](api-spec.md#register-account).

## 4. Start Testing the API

After registering accounts, you can test trading scenarios through the DEX.DO
API.

- Public REST API contract: [`api-spec.md`](api-spec.md).
- Generated OpenAPI contract: [`openapi.yaml`](openapi.yaml).
- Published API docs: <https://gosh-sh.github.io/dexdo/>.

Signed private and trading requests must include `X-DODEX-APIKEY`, `timestamp`,
and `signature` as described in
[`api-spec.md#security-types`](api-spec.md#security-types).
