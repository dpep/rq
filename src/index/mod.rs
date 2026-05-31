//! Indexing — incremental walker, indexer, and coverage tracking.
//!
//! TODO(phase-1): walk a checkout (respecting `.gitignore`), dispatch files to
//! the matching language plugin, persist symbols incrementally (skip unchanged
//! files via mtime/content_hash), and update coverage. Decoupled from search.
