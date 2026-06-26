#!/usr/bin/env python3
"""Thin DEX.DO REST client for the agent skills (read + trade).

Stdlib only (no pip installs). Speaks the public contract in
`docs/api-spec.md`: public market-data endpoints need no auth; private
endpoints are signed with HMAC-SHA256 per `## Security Types`.

Signing recipe (verified against the live backend):
  signature = HMAC_SHA256(canonicalQueryString + canonicalRequestBody, key)
  key                  = the apiSecret hex-DECODED to its 32 raw bytes
                         (the backend stores raw bytes; the hex is just transport)
  canonicalQueryString = every query pair except `signature`, percent-encoded
                         exactly as sent on the wire, sorted by key, joined with '&'
  canonicalRequestBody = the exact minified JSON body bytes sent, or '' for none

The same percent-encoded string is what we BOTH sign and put on the URL, so a
symbol like "PM — Foo#123-Yes" (spaces / em-dash / '#') signs and transmits
consistently.

Output: JSON to stdout (so the calling agent can render it however it likes).
On an API error the error envelope is printed to stdout and the process exits 1.
On a transport/usage error a small JSON {"error": "..."} goes to stderr, exit 2.
"""

import argparse
import hashlib
import hmac
import json
import os
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

DEFAULT_BASE_URL = os.environ.get("DEXDO_BASE_URL", "https://dodex-dev.ackinacki.org")
DEFAULT_RECV_WINDOW = 5000
# Trade endpoints submit an on-chain transaction before responding and can take
# tens of seconds; keep the client read timeout comfortably above that.
DEFAULT_HTTP_TIMEOUT = int(os.environ.get("DEXDO_HTTP_TIMEOUT", "90"))


def _fail(msg, code=2):
    json.dump({"error": msg}, sys.stderr)
    sys.stderr.write("\n")
    sys.exit(code)


def _now_ms():
    return int(time.time() * 1000)


def _encode_pairs(params):
    """[(k, v)] -> list of "k=urlencoded(v)" with v stringified.

    safe='' so ':' '#' ' ' and the em-dash all percent-encode — the exact
    bytes the backend will read back out of the raw query for the signature.
    """
    out = []
    for k, v in params:
        if v is None:
            continue
        out.append(f"{k}={urllib.parse.quote(str(v), safe='')}")
    return out


def load_creds(args):
    """Resolve apiKey + raw-byte HMAC key from --creds file or flags/env."""
    api_key = args.api_key or os.environ.get("DEXDO_API_KEY")
    api_secret = args.api_secret or os.environ.get("DEXDO_API_SECRET")
    creds_path = args.creds or os.environ.get("DEXDO_CREDS")
    if creds_path:
        try:
            with open(creds_path) as fh:
                blob = json.load(fh)
        except (OSError, ValueError) as exc:
            _fail(f"cannot read creds file {creds_path}: {exc}")
        api_key = api_key or blob.get("apiKey")
        api_secret = api_secret or blob.get("apiSecret")
    if not api_key or not api_secret:
        _fail("missing credentials: pass --creds FILE (or --api-key/--api-secret, "
              "or DEXDO_CREDS / DEXDO_API_KEY / DEXDO_API_SECRET)")
    try:
        key = bytes.fromhex(api_secret)
    except ValueError:
        _fail("apiSecret is not valid hex")
    return api_key, key


def _sign(key, canonical_qs, body_str):
    msg = (canonical_qs + body_str).encode("utf-8")
    return hmac.new(key, msg, hashlib.sha256).hexdigest()


def _request(method, url, timeout, headers=None, body_bytes=None):
    req = urllib.request.Request(url, method=method, data=body_bytes, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.getcode(), resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    except (TimeoutError, socket.timeout):
        # A trade may still have been accepted on-chain — surface a clear,
        # non-fatal hint instead of a stack trace. For POST /order, re-issue
        # with the SAME newOrderClientId; the exchange dedups.
        _fail(f"timed out after {timeout}s waiting for {url.split('?')[0]} — the "
              f"operation MAY have been accepted; verify via orders/account before retrying")
    except urllib.error.URLError as exc:
        _fail(f"transport error reaching {url.split('?')[0]}: {exc}")


def _is_error_envelope(obj):
    """The API error shape is `{"code": int, "msg": str}` (api-spec.md §Error)."""
    return isinstance(obj, dict) and "code" in obj and "msg" in obj


def _emit(status, text):
    """Print the JSON body; exit 1 if the API returned an error envelope."""
    try:
        parsed = json.loads(text)
    except ValueError:
        # Not JSON (e.g. an HTML error page) — surface raw.
        if status >= 400:
            _fail(f"HTTP {status}: {text[:400]}", code=1)
        print(text)
        return
    print(json.dumps(parsed, indent=2, ensure_ascii=False))
    if _is_error_envelope(parsed) or status >= 400:
        sys.exit(1)


def public_get(args, path, params):
    pairs = _encode_pairs(params)
    qs = "&".join(pairs)
    url = f"{args.base_url}{path}" + (f"?{qs}" if qs else "")
    return _request("GET", url, args.timeout)


def signed_request(args, method, path, query_params=None, body_obj=None):
    api_key, key = load_creds(args)
    query_params = list(query_params or [])
    query_params.append(("timestamp", _now_ms()))
    query_params.append(("recvWindow", args.recv_window))
    pairs = _encode_pairs(query_params)
    pairs.sort(key=lambda p: p.split("=", 1)[0])  # sort by key
    canonical_qs = "&".join(pairs)

    body_str = ""
    body_bytes = None
    headers = {"X-DODEX-APIKEY": api_key}
    if body_obj is not None:
        body_str = json.dumps(body_obj, separators=(",", ":"), ensure_ascii=False)
        body_bytes = body_str.encode("utf-8")
        headers["Content-Type"] = "application/json"

    sig = _sign(key, canonical_qs, body_str)
    url = f"{args.base_url}{path}?{canonical_qs}&signature={sig}"
    return _request(method, url, args.timeout, headers=headers, body_bytes=body_bytes)


# --------------------------- command handlers ---------------------------

def cmd_register(args):
    """POST /api/v1/accounts (public): register a deployed note, get + store creds.

    Reads the account body from --account-file (the onboarding `<tt>.account.json`),
    POSTs it, and — on success — writes the returned credential to --save-creds with
    owner-only perms (it carries the apiSecret, returned only once).
    """
    try:
        with open(args.account_file) as fh:
            body_obj = json.load(fh)
    except (OSError, ValueError) as exc:
        _fail(f"cannot read account file {args.account_file}: {exc}")
    body_str = json.dumps(body_obj, separators=(",", ":"), ensure_ascii=False)
    url = f"{args.base_url}/api/v1/accounts"
    status, text = _request("POST", url, args.timeout,
                            headers={"Content-Type": "application/json"},
                            body_bytes=body_str.encode("utf-8"))
    try:
        parsed = json.loads(text)
    except ValueError:
        _emit(status, text)
        return
    print(json.dumps(parsed, indent=2, ensure_ascii=False))
    if isinstance(parsed, dict) and parsed.get("apiSecret"):
        if args.save_creds:
            try:
                fd = os.open(args.save_creds, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                with os.fdopen(fd, "w") as fh:
                    json.dump(parsed, fh, indent=2)
                os.chmod(args.save_creds, 0o600)
                sys.stderr.write(f"saved credential (0600) to {args.save_creds}\n")
            except OSError as exc:
                _fail(f"got credential but failed to save it to {args.save_creds}: {exc}")
        else:
            sys.stderr.write("WARNING: apiSecret is returned ONCE — capture it now "
                             "(re-run with --save-creds <file> to store it)\n")
        return
    # error envelope (e.g. -2015 already registered, -2013 not deployed, -2016 wrong key)
    if _is_error_envelope(parsed) or status >= 400:
        sys.exit(1)


def cmd_markets(args):
    params = [
        ("predictionMarketAddress", args.market_address),
        ("status", args.status),
        ("quoteAsset", args.quote_asset),
        ("oracleName", args.oracle_name),
        ("sort", args.sort),
        ("cursor", args.cursor),
        ("limit", args.limit),
    ]
    _emit(*public_get(args, "/api/v1/prediction/markets", params))


def cmd_oracles(args):
    params = [
        ("oracleAddress", args.oracle_address),
        ("eventId", args.event_id),
        ("cursor", args.cursor),
        ("limit", args.limit),
    ]
    _emit(*public_get(args, "/api/v1/oracles", params))


def cmd_depth(args):
    params = [
        ("predictionMarketAddress", args.market_address),
        ("symbol", args.symbol),
        ("limit", args.limit),
    ]
    _emit(*public_get(args, "/api/v1/prediction/depth", params))


def cmd_trades(args):
    params = [
        ("predictionMarketAddress", args.market_address),
        ("symbol", args.symbol),
        ("limit", args.limit),
    ]
    _emit(*public_get(args, "/api/v1/prediction/trades", params))


def cmd_price(args):
    """Convenience: best bid/ask/mid/spread from depth + last public trade."""
    ds, dt = public_get(args, "/api/v1/prediction/depth",
                        [("predictionMarketAddress", args.market_address), ("symbol", args.symbol), ("limit", 1)])
    try:
        depth = json.loads(dt)
    except ValueError:
        _emit(ds, dt)
        return
    # depth is the required leg — if it errored, surface it and exit (same
    # contract as every other command via _emit).
    if ds >= 400 or _is_error_envelope(depth):
        _emit(ds, dt)
        return
    bids = depth.get("bids") or []
    asks = depth.get("asks") or []
    best_bid = bids[0][0] if bids else None
    best_ask = asks[0][0] if asks else None
    mid = spread = None
    if best_bid is not None and best_ask is not None:
        try:
            b, a = float(best_bid), float(best_ask)
            mid = f"{(a + b) / 2:.6f}"
            spread = f"{a - b:.6f}"
        except (TypeError, ValueError):
            mid = spread = None  # leave raw bid/ask; don't crash on odd values

    # trades is a best-effort leg: a failure degrades to lastTrade=null with a
    # note rather than failing the whole quote.
    ts, tt = public_get(args, "/api/v1/prediction/trades",
                        [("predictionMarketAddress", args.market_address), ("symbol", args.symbol), ("limit", 1)])
    last = None
    trades_error = None
    try:
        trades = json.loads(tt)
    except ValueError:
        trades = None
    if ts >= 400 or _is_error_envelope(trades):
        trades_error = trades if _is_error_envelope(trades) else f"HTTP {ts}"
    elif isinstance(trades, list) and trades:
        last = {"price": trades[0].get("price"), "qty": trades[0].get("qty"),
                "time": trades[0].get("time"), "isBuyerMaker": trades[0].get("isBuyerMaker")}
    out = {
        "predictionMarketAddress": args.market_address,
        "symbol": args.symbol,
        "bestBid": best_bid,
        "bestAsk": best_ask,
        "mid": mid,
        "spread": spread,
        "lastTrade": last,
        "lastUpdateId": depth.get("lastUpdateId"),
    }
    if trades_error is not None:
        out["tradesError"] = trades_error  # last-trade leg failed; bid/ask still valid
    print(json.dumps(out, indent=2, ensure_ascii=False))


def cmd_account(args):
    _emit(*signed_request(args, "GET", "/api/v1/account"))


def cmd_balances(args):
    _emit(*signed_request(args, "GET", "/api/v1/account/balances",
                          query_params=[("predictionMarketAddress", args.market_address)]))


def cmd_orders(args):
    status = args.status
    if args.open and not status:
        status = "NEW,PARTIALLY_FILLED"
    params = [
        ("predictionMarketAddress", args.market_address),
        ("symbol", args.symbol),
        ("status", status),
        ("limit", args.limit),
        ("cursor", args.cursor),
    ]
    _emit(*signed_request(args, "GET", "/api/v1/prediction/orders", query_params=params))


def cmd_order(args):
    body = {"predictionMarketAddress": args.market_address, "symbol": args.symbol, "side": args.side}
    if args.client_id:
        body["newOrderClientId"] = args.client_id
    body["quantity"] = args.quantity
    if args.price is not None:
        body["price"] = args.price
    if args.type:
        body["type"] = args.type
    if args.tif:
        body["timeInForce"] = args.tif
    _emit(*signed_request(args, "POST", "/api/v1/prediction/order", body_obj=body))


def cmd_cancel_order(args):
    params = [
        ("predictionMarketAddress", args.market_address),
        ("symbol", args.symbol),
        ("orderId", args.order_id),
    ]
    _emit(*signed_request(args, "DELETE", "/api/v1/prediction/order", query_params=params))


def cmd_buy_full_set(args):
    body = {"predictionMarketAddress": args.market_address, "collateral": args.collateral}
    _emit(*signed_request(args, "POST", "/api/v1/prediction/buyFullSet", body_obj=body))


def build_parser():
    # Common options live on a parent so they work after the subcommand
    # (`dexdo_client.py account --creds X`), the natural CLI order.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--base-url", default=DEFAULT_BASE_URL,
                        help=f"API base URL (default {DEFAULT_BASE_URL}; or $DEXDO_BASE_URL)")
    common.add_argument("--creds", help="path to a creds JSON ({apiKey, apiSecret}); or $DEXDO_CREDS")
    common.add_argument("--api-key", help="API key (overrides --creds); or $DEXDO_API_KEY")
    common.add_argument("--api-secret", help="API secret hex (overrides --creds); or $DEXDO_API_SECRET")
    common.add_argument("--recv-window", type=int, default=DEFAULT_RECV_WINDOW,
                        help=f"recvWindow ms (default {DEFAULT_RECV_WINDOW}, max 60000)")
    common.add_argument("--timeout", type=int, default=DEFAULT_HTTP_TIMEOUT,
                        help=f"HTTP read timeout seconds (default {DEFAULT_HTTP_TIMEOUT}; "
                             "or $DEXDO_HTTP_TIMEOUT)")

    # `common` is attached ONLY to the subparsers, not the root. If it were on
    # both, argparse would let the subparser's defaults overwrite a global option
    # given BEFORE the subcommand (`--base-url X markets` would silently revert to
    # the default host). Attaching to subparsers only means options must follow the
    # subcommand (`markets --base-url X`); using them before it fails loudly.
    p = argparse.ArgumentParser(
        description="DEX.DO REST client for agent skills (put options AFTER the subcommand)")
    sub = p.add_subparsers(dest="command", required=True)

    sp = sub.add_parser("register", help="POST /api/v1/accounts (public) — register a note, get creds",
                        parents=[common])
    sp.add_argument("--account-file", required=True,
                    help="path to the onboarding <tt>.account.json (the POST body)")
    sp.add_argument("--save-creds", help="write the returned credential here (mode 0600)")
    sp.set_defaults(func=cmd_register)

    sp = sub.add_parser("markets", help="GET /api/v1/prediction/markets (public)", parents=[common])
    sp.add_argument("--market-address")
    sp.add_argument("--status", help="comma list e.g. STAKING,TRADING")
    sp.add_argument("--quote-asset")
    sp.add_argument("--oracle-name")
    sp.add_argument("--sort", choices=["resultStart", "createdAt"])
    sp.add_argument("--cursor")
    sp.add_argument("--limit", type=int)
    sp.set_defaults(func=cmd_markets)

    sp = sub.add_parser("oracles", help="GET /api/v1/oracles (public)", parents=[common])
    sp.add_argument("--oracle-address")
    sp.add_argument("--event-id")
    sp.add_argument("--cursor")
    sp.add_argument("--limit", type=int)
    sp.set_defaults(func=cmd_oracles)

    sp = sub.add_parser("depth", help="GET /api/v1/prediction/depth (public)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--symbol", required=True)
    sp.add_argument("--limit", type=int)
    sp.set_defaults(func=cmd_depth)

    sp = sub.add_parser("trades", help="GET /api/v1/prediction/trades (public)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--symbol", required=True)
    sp.add_argument("--limit", type=int)
    sp.set_defaults(func=cmd_trades)

    sp = sub.add_parser("price", help="best bid/ask/mid/spread + last trade (public)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--symbol", required=True)
    sp.set_defaults(func=cmd_price)

    sp = sub.add_parser("account", help="GET /api/v1/account (signed)", parents=[common])
    sp.set_defaults(func=cmd_account)

    sp = sub.add_parser("balances", help="GET /api/v1/account/balances (signed)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.set_defaults(func=cmd_balances)

    sp = sub.add_parser("orders", help="GET /api/v1/prediction/orders (signed)", parents=[common])
    sp.add_argument("--market-address")
    sp.add_argument("--symbol")
    sp.add_argument("--status", help="comma list NEW,PARTIALLY_FILLED,FILLED,CANCELED,REJECTED")
    sp.add_argument("--open", action="store_true", help="shortcut for --status NEW,PARTIALLY_FILLED")
    sp.add_argument("--limit", type=int)
    sp.add_argument("--cursor")
    sp.set_defaults(func=cmd_orders)

    sp = sub.add_parser("order", help="POST /api/v1/prediction/order (signed, TRADE)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--symbol", required=True)
    sp.add_argument("--side", required=True, choices=["BUY", "SELL"])
    sp.add_argument("--quantity", required=True,
                    help="outcome qty; for MARKET BUY this is quote-asset spend")
    sp.add_argument("--price", help="required for LIMIT")
    sp.add_argument("--type", choices=["LIMIT", "MARKET"])
    sp.add_argument("--tif", choices=["GTC", "IOC", "FOK", "POST_ONLY"])
    sp.add_argument("--client-id")
    sp.set_defaults(func=cmd_order)

    sp = sub.add_parser("cancel-order", help="DELETE /api/v1/prediction/order (signed, TRADE)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--symbol", required=True)
    sp.add_argument("--order-id", required=True)
    sp.set_defaults(func=cmd_cancel_order)

    sp = sub.add_parser("buy-full-set", help="POST /api/v1/prediction/buyFullSet (signed, TRADE)", parents=[common])
    sp.add_argument("--market-address", required=True)
    sp.add_argument("--collateral", required=True, help="quote-asset amount to split")
    sp.set_defaults(func=cmd_buy_full_set)

    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
