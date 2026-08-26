-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- The public API served a model as four fields — `producer`, `name`, `version`
-- and `ref` — where the last three columns below were the first three of them.
-- It now serves one, `modelRefName`, which is `model_ref`: the on-chain name
-- verbatim.
--
-- The split was only ever a guess at structure. It required the name to be
-- exactly three `--`-separated parts and produced three NULLs for anything else,
-- and the model registry has since been re-seeded with names that are not in
-- that shape at all — so the columns would have been NULL for every new market.
--
-- Nothing is lost that cannot be recovered: these values were derived from
-- `model_ref`, which stays, and `model_ref` itself is re-read from the book's
-- `getModelName()` getter on every reconcile.

alter table inference_markets
    drop column producer,
    drop column model_name,
    drop column model_version;
