// Re-exports for integration tests that need to assemble the
// production router and app state against a test-DB pool. The module
// is `#[doc(hidden)]` and every item is `#[doc(hidden)]` re-exported,
// so generated documentation surfaces only `run` as the supported
// public API; production binaries link these symbols but never call
// them through this path.

#[doc(hidden)]
pub use crate::build_router;
#[doc(hidden)]
pub use crate::AppState;
#[doc(hidden)]
pub use crate::SharedAuth;
#[doc(hidden)]
pub use crate::SharedChainSender;
#[doc(hidden)]
pub use crate::SharedRepo;
