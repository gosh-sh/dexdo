# DEX.DO

Copyright (C) 2026 GOSH TECHNOLOGY LTD.

DEX.DO is free software: you can redistribute it and/or modify it under the
terms of the **GNU Affero General Public License**, version 3, as published
by the Free Software Foundation. The full license text is in
[LICENSE.md](LICENSE.md).

DEX.DO is distributed in the hope that it will be useful, but **WITHOUT ANY
WARRANTY**; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License for
more details.

## Runtime dependency: Acki Nacki Block Manager

DEX.DO is a plugin for the **Acki Nacki Block Manager** node software. It
cannot run on its own — it consumes the chain that the Block Manager
indexes, signs and dispatches transactions via the Block Manager's gateway,
and reads contract state through it.

The Acki Nacki Block Manager software is published by
**GOSH TECHNOLOGY LTD.** under a separate license — the **Acki Nacki Node
License (ANNL)**, a Business Source License with a two-year change date to
GNU AGPL-3.0. The ANNL covers the Block Manager software itself, not
DEX.DO. Refer to the Block Manager repository for its current license text
and terms.

The AGPL terms of DEX.DO apply only to DEX.DO itself — the project's own
source code, builds, deployments, and modifications. They do not extend to
or override the licensing of any separately distributed software (such as
the Block Manager) that DEX.DO interoperates with at runtime.
